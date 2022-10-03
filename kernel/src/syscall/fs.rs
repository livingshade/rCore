//! Syscalls for file system

#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::cmp::min;
use core::mem::size_of;
#[cfg(not(target_arch = "mips"))]
use rcore_fs::vfs::Timespec;

use crate::drivers::SOCKET_ACTIVITY;
use crate::fs::*;
use crate::memory::MemorySet;
use crate::sync::Condvar;
use crate::trap::TICK_ACTIVITY;
use alloc::boxed::Box;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use bitvec::prelude::{BitSlice, BitVec, Lsb0};

use super::*;
use crate::fs::epoll::EpollInstance;
use crate::fs::fcntl::{FD_CLOEXEC, F_SETFD, O_CLOEXEC, O_NONBLOCK};
use crate::fs::FileLike;
use crate::process::Process;
use crate::syscall::SysError::{EINTR, EINVAL, ESPIPE};
use rcore_fs::vfs::PollStatus;

impl Syscall<'_> {
    pub async fn sys_read(&mut self, fd: usize, base: UserOutPtr<u8>, len: usize) -> SysResult {
        let mut proc = self.process();
        if !proc.pid.is_init() {
            // we trust pid 0 process
            info!("read: fd: {}, base: {:?}, len: {:#x}", fd, base, len);
        }
        let slice = unsafe { self.vm().check_write_array(base.ptr(), len)? };

        let file_like = proc.get_file_like(fd)?;
        let len = file_like.read(slice).await?;
        Ok(len)
    }

    pub fn sys_write(&mut self, fd: usize, base: *const u8, len: usize) -> SysResult {
        let mut proc = self.process();
        if !proc.pid.is_init() {
            //we trust pid 0 process
            info!("write: fd: {}, base: {:?}, len: {:#x}", fd, base, len);
        }
        let slice = unsafe { self.vm().check_read_array(base, len)? };
        let file_like = proc.get_file_like(fd)?;
        let len = file_like.write(slice)?;
        Ok(len)
    }

    pub async fn sys_pread(
        &mut self,
        fd: usize,
        mut base: UserOutPtr<u8>,
        len: usize,
        offset: usize,
    ) -> SysResult {
        info!(
            "pread: fd: {}, base: {:?}, len: {}, offset: {}",
            fd, base, len, offset
        );
        let mut proc = self.process();
        let slice = unsafe { self.vm().check_write_array(base.ptr(), len)? };
        let len = proc.get_file(fd)?.read_at(offset, slice).await?;
        Ok(len)
    }

    pub fn sys_pwrite(
        &mut self,
        fd: usize,
        base: *const u8,
        len: usize,
        offset: usize,
    ) -> SysResult {
        info!(
            "pwrite: fd: {}, base: {:?}, len: {}, offset: {}",
            fd, base, len, offset
        );
        let mut proc = self.process();
        let slice = unsafe { self.vm().check_read_array(base, len)? };
        let len = proc.get_file(fd)?.write_at(offset, slice)?;
        Ok(len)
    }

    /// sys_ppoll function is for handling the third argument of sys_poll.

    pub async fn sys_ppoll(
        &mut self,
        ufds: UserInOutPtr<PollFd>,
        nfds: usize,
        timeout: UserInPtr<TimeSpec>,
    ) -> SysResult {
        let proc = self.process();
        if !proc.pid.is_init() {
            info!(
                "ppoll: ufds: {:?} nfds: {}, timeout: {:?}",
                ufds, nfds, timeout
            );
        }
        let timeout_msecs = if timeout.is_null() {
            1 << 31 // infinity
        } else {
            let timeout = timeout.read().unwrap();
            timeout.to_msec()
        };
        drop(proc);

        self.sys_poll(ufds, nfds, timeout_msecs as usize).await
    }

    pub async fn sys_poll(
        &mut self,
        mut ufds: UserInOutPtr<PollFd>,
        nfds: usize,
        timeout_msecs: usize,
    ) -> SysResult {
        let proc = self.process();
        if !proc.pid.is_init() {
            // we trust pid 0 process
            info!(
                "poll: ufds: {:?}, nfds: {}, timeout_msecs: {:#x}",
                ufds, nfds, timeout_msecs
            );
        }

        // check whether the fds is valid and is owned by this process
        let condvars = alloc::vec![&(*TICK_ACTIVITY), &(*SOCKET_ACTIVITY)];

        let mut polls = ufds.read_array(nfds).unwrap();

        if !proc.pid.is_init() {
            info!("poll: fds: {:?}", polls);
        }

        drop(proc);

        #[must_use = "future does nothing unless polled/`await`-ed"]
        struct PollFuture<'a> {
            polls: &'a mut Vec<PollFd>,
            syscall: &'a Syscall<'a>,
        }

        impl<'a> Future for PollFuture<'a> {
            type Output = SysResult;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                use PollEvents as PE;
                let proc = self.syscall.process();
                let mut events = 0;

                // iterate each poll to check whether it is ready
                for poll in self.as_mut().polls.iter_mut() {
                    poll.revents = PE::empty();
                    if let Some(file_like) = proc.files.get(&(poll.fd as usize)) {
                        let mut fut = Box::pin(file_like.async_poll());
                        let status = match fut.as_mut().poll(cx) {
                            Poll::Ready(Ok(ret)) => ret,
                            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                            Poll::Pending => continue,
                        };
                        if status.error {
                            poll.revents |= PE::HUP;
                            events += 1;
                        }
                        if status.read && poll.events.contains(PE::IN) {
                            poll.revents |= PE::IN;
                            events += 1;
                        }
                        if status.write && poll.events.contains(PE::OUT) {
                            poll.revents |= PE::OUT;
                            events += 1;
                        }
                    } else {
                        poll.revents |= PE::ERR;
                        events += 1;
                    }
                }
                drop(proc);

                // some event happens, so evoke the process
                if events > 0 {
                    return Poll::Ready(Ok(events));
                }

                return Poll::Pending;
            }
        }

        let future = PollFuture {
            polls: &mut polls,
            syscall: self,
        };
        let res = future.await;
        ufds.write_array(&polls)?;
        res
    }

    pub fn sys_pselect6(
        &mut self,
        nfds: usize,
        read: *mut u32,
        write: *mut u32,
        err: *mut u32,
        timeout: *const TimeVal,
        _sigset: *const u32,
    ) -> SysResult {
        self.sys_select(nfds, read, write, err, timeout)
    }

    pub fn sys_select(
        &mut self,
        nfds: usize,
        read: *mut u32,
        write: *mut u32,
        err: *mut u32,
        timeout: *const TimeVal,
    ) -> SysResult {
        info!(
            "select: nfds: {}, read: {:?}, write: {:?}, err: {:?}, timeout: {:?}",
            nfds, read, write, err, timeout
        );
        if nfds as u64 == 0 {
            return Ok(0);
        }
        let proc = self.process();
        let mut read_fds = FdSet::new(&self.vm(), read, nfds)?;
        let mut write_fds = FdSet::new(&self.vm(), write, nfds)?;
        let mut err_fds = FdSet::new(&self.vm(), err, nfds)?;
        let timeout_msecs = if !timeout.is_null() {
            let timeout = unsafe { self.vm().check_read_ptr(timeout)? };
            timeout.to_msec()
        } else {
            // infinity
            1 << 31
        };

        let condvars = alloc::vec![&(*TICK_ACTIVITY), &(*SOCKET_ACTIVITY)];

        // for debugging
        if cfg!(debug_assertions) {
            debug!("files before select {:#?}", proc.files);
        }
        drop(proc);

        let begin_time_ms = crate::trap::uptime_msec();
        Condvar::wait_events(condvars.as_slice(), move || {
            let proc = self.process();
            let mut events = 0;
            for (&fd, file_like) in proc.files.iter() {
                //                if fd >= nfds {
                //                    continue;
                //                }
                if !err_fds.contains(fd) && !read_fds.contains(fd) && !write_fds.contains(fd) {
                    continue;
                }
                let status = match file_like.poll() {
                    Ok(ret) => ret,
                    Err(err) => return Some(Err(err)),
                };
                if status.error && err_fds.contains(fd) {
                    err_fds.set(fd);
                    events += 1;
                }
                if status.read && read_fds.contains(fd) {
                    read_fds.set(fd);
                    events += 1;
                }
                if status.write && write_fds.contains(fd) {
                    write_fds.set(fd);
                    events += 1;
                }
            }
            drop(proc);

            if events > 0 {
                return Some(Ok(events));
            }

            if timeout_msecs == 0 {
                // no timeout, return now;
                return Some(Ok(0));
            }

            let current_time_ms = crate::trap::uptime_msec();
            // infinity check
            if timeout_msecs < (1 << 31) && current_time_ms - begin_time_ms > timeout_msecs as usize
            {
                return Some(Ok(0));
            }

            return None;
        })
    }

    pub fn sys_epoll_create(&mut self, size: usize) -> SysResult {
        info!("epoll_create: size: {:?}", size);

        if (size as i32) < 0 {
            return Err(SysError::EINVAL);
        }
        self.sys_epoll_create1(0)
    }

    pub fn sys_epoll_create1(&mut self, flags: usize) -> SysResult {
        info!("epoll_create1: flags: {:?}", flags);
        let mut proc = self.process();
        let epoll_instance = EpollInstance::new(flags);
        let fd = proc.add_file(FileLike::EpollInstance(epoll_instance));
        Ok(fd)
    }

    pub fn sys_epoll_ctl(
        &mut self,
        epfd: usize,
        op: usize,
        fd: usize,
        event: *mut EpollEvent,
    ) -> SysResult {
        let mut proc = self.process();
        if !proc.pid.is_init() {
            // we trust pid 0 process
            info!("sys_epoll_ctl: epfd: {}, op: {:?}, fd: {:#x}", epfd, op, fd);
        }

        let _event = unsafe { self.vm().check_read_ptr(event)? };

        if proc.files.get(&fd).is_none() {
            return Err(SysError::EPERM);
        }

        let epoll_instance = match proc.get_epoll_instance_mut(epfd) {
            Ok(ins) => ins,
            Err(err) => {
                return Err(err);
            }
        };

        let ret = epoll_instance.control(op, fd, &_event)?;
        return Ok(ret);
    }

    pub fn sys_epoll_wait(
        &mut self,
        epfd: usize,
        events: *mut EpollEvent,
        maxevents: usize,
        timeout: usize,
    ) -> SysResult {
        self.sys_epoll_pwait(epfd, events, maxevents, timeout, 0)
    }

    pub fn sys_epoll_pwait(
        &mut self,
        epfd: usize,
        events: *mut EpollEvent,
        maxevents: usize,
        timeout_msecs: usize,
        _sigset_t: usize,
    ) -> SysResult {
        info!("epoll_pwait: epfd: {}, timeout: {:?}", epfd, timeout_msecs);

        let proc = self.process();
        let events = unsafe { self.vm().check_write_array(events, maxevents)? };
        let epoll_instance = proc.get_epoll_instance(epfd)?;

        // add new fds which are registered by epoll_ctl after latest epoll_pwait
        epoll_instance.ready_list.lock().clear();
        epoll_instance
            .ready_list
            .lock()
            .extend(epoll_instance.new_ctl_list.lock().clone());
        epoll_instance.new_ctl_list.lock().clear();

        // if registered fd has data to handle and its mode isn't epollet, we need
        // to add it to the list.
        let keys: Vec<_> = epoll_instance.events.keys().cloned().collect();
        for (k, v) in epoll_instance.events.iter() {
            if !v.contains(EpollEvent::EPOLLET) {
                match &proc.files.get(k) {
                    None => {
                        //      return Err(SysError::EINVAL);
                    }
                    Some(file_like) => {
                        let status = file_like.poll()?;
                        if status.write || status.read || status.error {
                            let mut ready_list = epoll_instance.ready_list.lock();
                            ready_list.insert(*k);
                        }
                    }
                }
            }
        }
        drop(proc);

        let mut callbacks = alloc::vec![];
        for fd in &keys {
            let proc = self.process();
            match proc.files.get(&fd) {
                Some(file_like) => {
                    match file_like {
                        FileLike::File(_file) => {
                            callbacks.push((
                                0, 0, // thread::current().id(),
                                epfd, *fd,
                            ));
                        }
                        FileLike::Socket(_socket) => {
                            let _ = &(*crate::drivers::SOCKET_ACTIVITY).register_epoll_list(
                                self.thread.proc.clone(),
                                0,
                                //thread::current().id(),
                                epfd,
                                *fd,
                            );
                            callbacks.push((
                                1, 0, //thread::current().id(),
                                epfd, *fd,
                            ));
                        }
                        FileLike::EpollInstance(_) => {
                            return Err(SysError::EINVAL);
                        }
                    };
                }
                None => {}
            }
            drop(proc);
        }

        let condvars = alloc::vec![&(*TICK_ACTIVITY), &(*SOCKET_ACTIVITY)];

        let begin_time_ms = crate::trap::uptime_msec();
        let condition = move || {
            let mut proc = self.process();

            let epoll_instance = match proc.get_epoll_instance_mut(epfd) {
                Ok(ins) => ins,
                Err(err) => {
                    return Some(Err(err));
                }
            };
            let ready_list = epoll_instance.ready_list.lock().clone();
            let mut events_num = 0;

            for infd in ready_list.iter() {
                let mut status: PollStatus = Default::default();
                {
                    if let Some(file_like) = proc.files.get(&infd) {
                        let _status = match file_like.poll() {
                            Ok(ret) => ret,
                            Err(err) => return Some(Err(err)),
                        };
                        status.write = _status.write;
                        status.read = _status.read;
                        status.error = _status.error;
                    }
                }

                {
                    let epoll_instance = match proc.get_epoll_instance_mut(epfd) {
                        Ok(ins) => ins,
                        Err(err) => {
                            return Some(Err(err));
                        }
                    };
                    let epollevent = epoll_instance.events.get_mut(&infd)?;

                    if status.error {
                        events[events_num].events = EpollEvent::EPOLLERR;
                        events[events_num].data = epollevent.data;

                        events_num += 1;
                    }
                    if status.read && epollevent.contains(EpollEvent::EPOLLIN) {
                        events[events_num].events = EpollEvent::EPOLLIN;
                        events[events_num].data = epollevent.data;
                        events_num += 1;
                    }
                    if status.write && epollevent.contains(EpollEvent::EPOLLOUT) {
                        events[events_num].events = EpollEvent::EPOLLOUT;
                        events[events_num].data = epollevent.data;

                        events_num += 1;
                    }
                }
            }

            {
                let epoll_instance = match proc.get_epoll_instance_mut(epfd) {
                    Ok(ins) => ins,
                    Err(err) => {
                        return Some(Err(err));
                    }
                };
                epoll_instance.ready_list.lock().clear();
            }

            drop(proc);

            // some event happens, so evoke the process
            if events_num > 0 {
                return Some(Ok(events_num));
            }

            let current_time_ms = crate::trap::uptime_msec();
            // time runs out, so the evoke the process
            if timeout_msecs < (1 << 31) && current_time_ms - begin_time_ms > timeout_msecs {
                return Some(Ok(0));
            }
            return None;
        };

        let num = Condvar::wait_events(condvars.as_slice(), condition).unwrap();

        for cb in callbacks.iter() {
            match cb.0 {
                1 => &(*crate::drivers::SOCKET_ACTIVITY).unregister_epoll_list(cb.1, cb.2, cb.3),
                _ => panic!("cb error"),
            };
        }
        Ok(num)
    }

    pub async fn sys_readv(
        &mut self,
        fd: usize,
        iov_ptr: UserInPtr<IoVec>,
        iov_count: usize,
    ) -> SysResult {
        info!(
            "readv: fd: {}, iov: {:?}, count: {}",
            fd, iov_ptr, iov_count
        );
        let mut proc = self.process();
        let mut iovs =
            unsafe { IoVecs::check_and_new(iov_ptr.ptr(), iov_count, &self.vm(), true)? };

        // read all data to a buf
        let file_like = proc.get_file_like(fd)?;
        let mut buf = iovs.new_buf(true);
        let len = file_like.read(buf.as_mut_slice()).await?;
        // copy data to user
        iovs.write_all_from_slice(&buf[..len]);
        Ok(len)
    }

    pub fn sys_writev(&mut self, fd: usize, iov_ptr: *const IoVec, iov_count: usize) -> SysResult {
        let mut proc = self.process();
        if !proc.pid.is_init() {
            // we trust pid 0 process
            info!(
                "writev: fd: {}, iov: {:?}, count: {}",
                fd, iov_ptr, iov_count
            );
        } else {
            // info!(
            //     "writev: fd: {}, iov: {:?}, count: {}",
            //     fd, iov_ptr, iov_count
            // );
        }
        let iovs = unsafe { IoVecs::check_and_new(iov_ptr, iov_count, &self.vm(), false)? };

        let buf = iovs.read_all_to_vec();
        let file_like = proc.get_file_like(fd)?;
        let len = file_like.write(buf.as_slice())?;
        Ok(len)
    }

    pub fn sys_open(&mut self, path: *const u8, flags: usize, mode: usize) -> SysResult {
        self.sys_openat(AT_FDCWD, path, flags, mode)
    }

    pub fn sys_openat(
        &mut self,
        dir_fd: usize,
        path: *const u8,
        flags: usize,
        mode: usize,
    ) -> SysResult {
        let mut proc = self.process();
        let path = check_and_clone_cstr(path)?;
        let flags = OpenFlags::from_bits_truncate(flags);
        info!(
            "openat: dir_fd: {}, path: {:?}, flags: {:?}, mode: {:#o}",
            dir_fd as isize, path, flags, mode
        );

        let inode = if flags.contains(OpenFlags::CREATE) {
            let (dir_path, file_name) = split_path(&path);
            // relative to cwd
            let dir_inode = proc.lookup_inode_at(dir_fd, dir_path, true)?;
            match dir_inode.find(file_name) {
                Ok(file_inode) => {
                    if flags.contains(OpenFlags::EXCLUSIVE) {
                        return Err(SysError::EEXIST);
                    }
                    if flags.contains(OpenFlags::TRUNCATE) {
                        if let Err(e) = file_inode.resize(0) {
                            // TODO: do something? what about device file?
                        }
                    }
                    file_inode
                }
                Err(FsError::EntryNotFound) => {
                    let inode = dir_inode.create(file_name, FileType::File, mode as u32)?;
                    TimeSpec::update(&inode);
                    TimeSpec::update(&dir_inode);
                    inode
                }
                Err(e) => return Err(SysError::from(e)),
            }
        } else {
            proc.lookup_inode_at(dir_fd, &path, true)?
        };

        let file = FileHandle::new(
            inode,
            flags.to_options(),
            String::from(path),
            false,
            flags.contains(OpenFlags::CLOEXEC),
        );

        // for debugging
        if cfg!(debug_assertions) {
            debug!("files before open {:#?}", proc.files);
        }

        let fd = proc.add_file(FileLike::File(file));
        Ok(fd)
    }

    pub fn sys_close(&mut self, fd: usize) -> SysResult {
        info!("close: fd: {:?}", fd);
        let mut proc = self.process();

        // for debugging
        if cfg!(debug_assertions) {
            debug!("files before close {:#?}", proc.files);
        }

        proc.files.remove(&fd).ok_or(SysError::EBADF)?;
        Ok(0)
    }

    pub fn sys_access(&mut self, path: *const u8, mode: usize) -> SysResult {
        self.sys_faccessat(AT_FDCWD, path, mode, 0)
    }

    pub fn sys_faccessat(
        &mut self,
        dirfd: usize,
        path: *const u8,
        mode: usize,
        flags: usize,
    ) -> SysResult {
        // TODO: check permissions based on uid/gid
        let proc = self.process();
        let path = check_and_clone_cstr(path)?;
        let flags = AtFlags::from_bits_truncate(flags);
        if !proc.pid.is_init() {
            // we trust pid 0 process
            info!(
                "faccessat: dirfd: {}, path: {:?}, mode: {:#o}, flags: {:?}",
                dirfd as isize, path, mode, flags
            );
        }
        let _inode =
            proc.lookup_inode_at(dirfd, &path, !flags.contains(AtFlags::SYMLINK_NOFOLLOW))?;
        Ok(0)
    }

    pub fn sys_getcwd(&mut self, buf: *mut u8, len: usize) -> SysResult {
        let proc = self.process();
        if !proc.pid.is_init() {
            // we trust pid 0 process
            info!("getcwd: buf: {:?}, len: {:#x}", buf, len);
        }
        let buf = unsafe { self.vm().check_write_array(buf, len)? };
        if proc.cwd.len() + 1 > len {
            return Err(SysError::ERANGE);
        }
        unsafe { util::write_cstr(buf.as_mut_ptr(), &proc.cwd) }
        Ok(buf.as_ptr() as usize)
    }

    pub fn sys_lstat(&mut self, path: *const u8, stat_ptr: *mut Stat) -> SysResult {
        self.sys_fstatat(AT_FDCWD, path, stat_ptr, AtFlags::SYMLINK_NOFOLLOW.bits())
    }

    pub fn sys_fstat(&mut self, fd: usize, stat_ptr: *mut Stat) -> SysResult {
        info!("fstat: fd: {}, stat_ptr: {:?}", fd, stat_ptr);
        let mut proc = self.process();
        let stat_ref = unsafe { self.vm().check_write_ptr(stat_ptr)? };
        let file = proc.get_file(fd)?;
        let metadata = file.metadata()?;
        let stat = Stat::from(metadata);
        *stat_ref = stat;
        Ok(0)
    }

    pub fn sys_fstatat(
        &mut self,
        dirfd: usize,
        path: *const u8,
        stat_ptr: *mut Stat,
        flags: usize,
    ) -> SysResult {
        let proc = self.process();
        let path = check_and_clone_cstr(path)?;
        let stat_ref = unsafe { self.vm().check_write_ptr(stat_ptr)? };
        let flags = AtFlags::from_bits_truncate(flags);
        info!(
            "fstatat: dirfd: {}, path: {:?}, stat_ptr: {:?}, flags: {:?}",
            dirfd as isize, path, stat_ptr, flags
        );

        let inode =
            proc.lookup_inode_at(dirfd, &path, !flags.contains(AtFlags::SYMLINK_NOFOLLOW))?;
        let stat = Stat::from(inode.metadata()?);
        *stat_ref = stat;
        Ok(0)
    }

    pub fn sys_stat(&mut self, path: *const u8, stat_ptr: *mut Stat) -> SysResult {
        self.sys_fstatat(AT_FDCWD, path, stat_ptr, 0)
    }

    pub fn sys_readlink(&mut self, path: *const u8, base: *mut u8, len: usize) -> SysResult {
        self.sys_readlinkat(AT_FDCWD, path, base, len)
    }

    pub fn sys_readlinkat(
        &mut self,
        dirfd: usize,
        path: *const u8,
        base: *mut u8,
        len: usize,
    ) -> SysResult {
        let proc = self.process();
        let path = check_and_clone_cstr(path)?;
        let slice = unsafe { self.vm().check_write_array(base, len)? };
        info!(
            "readlinkat: dirfd: {}, path: {:?}, base: {:?}, len: {}",
            dirfd as isize, path, base, len
        );

        let inode = proc.lookup_inode_at(dirfd, &path, false)?;
        if inode.metadata()?.type_ == FileType::SymLink {
            // TODO: recursive link resolution and loop detection
            let len = inode.read_at(0, slice)?;
            Ok(len)
        } else {
            Err(SysError::EINVAL)
        }
    }

    pub fn sys_lseek(&mut self, fd: usize, offset: i64, whence: u8) -> SysResult {
        let pos = match whence {
            SEEK_SET => SeekFrom::Start(offset as u64),
            SEEK_END => SeekFrom::End(offset),
            SEEK_CUR => SeekFrom::Current(offset),
            _ => return Err(SysError::EINVAL),
        };
        info!("lseek: fd: {}, pos: {:?}", fd, pos);

        let mut proc = self.process();
        let file = proc.get_file(fd)?;
        if file.pipe {
            Err(ESPIPE)
        } else {
            let offset = file.seek(pos)?;
            Ok(offset as usize)
        }
    }

    pub fn sys_fsync(&mut self, fd: usize) -> SysResult {
        info!("fsync: fd: {}", fd);
        self.process().get_file(fd)?.sync_all()?;
        Ok(0)
    }

    pub fn sys_flock(&mut self, fd: usize, operation: usize) -> SysResult {
        bitflags! {
            struct Operation: u8 {
                const LOCK_SH = 1;
                const LOCK_EX = 2;
                const LOCK_NB = 4;
                const LOCK_UN = 8;
            }
        }
        let operation = Operation::from_bits(operation as u8).unwrap();
        info!("flock: fd: {}, operation: {:?}", fd, operation);
        let mut proc = self.process();
        // let file_like = proc.get_file_like(fd)?;
        let file = proc.get_file(fd)?;
        Ok(0)
    }

    pub fn sys_fdatasync(&mut self, fd: usize) -> SysResult {
        info!("fdatasync: fd: {}", fd);
        self.process().get_file(fd)?.sync_data()?;
        Ok(0)
    }

    pub fn sys_truncate(&mut self, path: *const u8, len: usize) -> SysResult {
        let proc = self.process();
        let path = check_and_clone_cstr(path)?;
        info!("truncate: path: {:?}, len: {}", path, len);
        proc.lookup_inode(&path)?.resize(len)?;
        Ok(0)
    }

    pub fn sys_ftruncate(&mut self, fd: usize, len: usize) -> SysResult {
        info!("ftruncate: fd: {}, len: {}", fd, len);
        self.process().get_file(fd)?.set_len(len as u64)?;
        Ok(0)
    }

    pub fn sys_getdents64(
        &mut self,
        fd: usize,
        buf: *mut LinuxDirent64,
        buf_size: usize,
    ) -> SysResult {
        info!(
            "getdents64: fd: {}, ptr: {:?}, buf_size: {}",
            fd, buf, buf_size
        );
        let mut proc = self.process();
        let buf = unsafe { self.vm().check_write_array(buf as *mut u8, buf_size)? };
        let file = proc.get_file(fd)?;
        let info = file.metadata()?;
        if info.type_ != FileType::Dir {
            return Err(SysError::ENOTDIR);
        }
        let mut writer = DirentBufWriter::new(buf);
        loop {
            let (info, name) = match file.read_entry_with_metadata() {
                Err(FsError::EntryNotFound) => break,
                r => r,
            }?;
            let ok = writer.try_write(
                info.inode as u64,
                DirentType::from_type(&info.type_).bits(),
                &name,
            );
            if !ok {
                file.seek(SeekFrom::Current(-1))?;
                break;
            }
        }
        Ok(writer.written_size)
    }

    pub fn sys_dup2(&mut self, fd1: usize, fd2: usize) -> SysResult {
        info!("dup2: from {} to {}", fd1, fd2);
        self.dup_impl(fd1, fd2, 0)
    }

    fn dup_impl(&mut self, fd1: usize, fd2: usize, flags: usize) -> SysResult {
        let mut proc = self.process();
        // close fd2 first if it is opened
        proc.files.remove(&fd2);

        let mut file_like = proc.get_file_like(fd1)?.dup(flags != 0);
        proc.files.insert(fd2, file_like);
        Ok(fd2)
    }

    pub fn sys_dup3(&mut self, fd1: usize, fd2: usize, flags: usize) -> SysResult {
        info!("dup3: from {} to {} with flags = {:#x}", fd1, fd2, flags);
        self.dup_impl(fd1, fd2, flags)
    }

    pub fn sys_ioctl(
        &mut self,
        fd: usize,
        request: usize,
        arg1: usize,
        arg2: usize,
        arg3: usize,
    ) -> SysResult {
        info!(
            "ioctl: fd: {}, request: {:#x}, args: {:#x} {:#x} {:#x}",
            fd, request, arg1, arg2, arg3
        );
        use crate::fs::ioctl::*;
        match request {
            FIOCLEX => self.sys_fcntl(fd, F_SETFD, FD_CLOEXEC),
            FIONCLEX => self.sys_fcntl(fd, F_SETFD, 0),
            FIONBIO => {
                let data = arg1 as *const i32;
                let val = unsafe { *data };
                if val == 0 {
                    self.sys_fcntl(fd, F_SETFD, 0)
                } else {
                    self.sys_fcntl(fd, F_SETFD, O_NONBLOCK)
                }
            }
            _ => {
                let mut proc = self.process();
                let file_like = proc.get_file_like(fd)?;
                file_like.ioctl(request, arg1, arg2, arg3)
            }
        }
    }

    pub fn sys_chdir(&mut self, path: *const u8) -> SysResult {
        let mut proc = self.process();
        let path = check_and_clone_cstr(path)?;
        if !proc.pid.is_init() {
            // we trust pid 0 process
            info!("chdir: path: {:?}", path);
        }

        let inode = proc.lookup_inode(&path)?;
        let info = inode.metadata()?;
        if info.type_ != FileType::Dir {
            return Err(SysError::ENOTDIR);
        }

        // BUGFIX: '..' and '.'
        if path.len() > 0 {
            let cwd = match path.as_bytes()[0] {
                b'/' => String::from("/"),
                _ => proc.cwd.clone(),
            };
            let mut cwd_vec: Vec<_> = cwd.split("/").filter(|&x| x != "").collect();
            let path_split = path.split("/").filter(|&x| x != "");
            for seg in path_split {
                if seg == ".." {
                    cwd_vec.pop();
                } else if seg == "." {
                    // nothing to do here.
                } else {
                    cwd_vec.push(seg);
                }
            }
            proc.cwd = String::from("");
            for seg in cwd_vec {
                proc.cwd.push_str("/");
                proc.cwd.push_str(seg);
            }
            if proc.cwd == "" {
                proc.cwd = String::from("/");
            }
        }
        Ok(0)
    }

    pub fn sys_rename(&mut self, oldpath: *const u8, newpath: *const u8) -> SysResult {
        self.sys_renameat(AT_FDCWD, oldpath, AT_FDCWD, newpath)
    }

    pub fn sys_renameat(
        &mut self,
        olddirfd: usize,
        oldpath: *const u8,
        newdirfd: usize,
        newpath: *const u8,
    ) -> SysResult {
        let proc = self.process();
        let oldpath = check_and_clone_cstr(oldpath)?;
        let newpath = check_and_clone_cstr(newpath)?;
        info!(
            "renameat: olddirfd: {}, oldpath: {:?}, newdirfd: {}, newpath: {:?}",
            olddirfd as isize, oldpath, newdirfd as isize, newpath
        );

        let (old_dir_path, old_file_name) = split_path(&oldpath);
        let (new_dir_path, new_file_name) = split_path(&newpath);
        let old_dir_inode = proc.lookup_inode_at(olddirfd, old_dir_path, false)?;
        let new_dir_inode = proc.lookup_inode_at(newdirfd, new_dir_path, false)?;
        old_dir_inode.move_(old_file_name, &new_dir_inode, new_file_name)?;
        Ok(0)
    }

    pub fn sys_mkdir(&mut self, path: *const u8, mode: usize) -> SysResult {
        self.sys_mkdirat(AT_FDCWD, path, mode)
    }

    pub fn sys_mkdirat(&mut self, dirfd: usize, path: *const u8, mode: usize) -> SysResult {
        let proc = self.process();
        let path = check_and_clone_cstr(path)?;
        // TODO: check pathname
        info!(
            "mkdirat: dirfd: {}, path: {:?}, mode: {:#o}",
            dirfd as isize, path, mode
        );

        let (dir_path, file_name) = split_path(&path);
        let dir_inode = proc.lookup_inode_at(dirfd, dir_path, true)?;
        if dir_inode.find(file_name).is_ok() {
            return Err(SysError::EEXIST);
        }
        let inode = dir_inode.create(file_name, FileType::Dir, mode as u32)?;
        TimeSpec::update(&inode);
        TimeSpec::update(&dir_inode);
        Ok(0)
    }

    pub fn sys_rmdir(&mut self, path: *const u8) -> SysResult {
        let proc = self.process();
        let path = check_and_clone_cstr(path)?;
        info!("rmdir: path: {:?}", path);

        let (dir_path, file_name) = split_path(&path);
        let dir_inode = proc.lookup_inode(dir_path)?;
        let file_inode = dir_inode.find(file_name)?;
        if file_inode.metadata()?.type_ != FileType::Dir {
            return Err(SysError::ENOTDIR);
        }
        dir_inode.unlink(file_name)?;
        Ok(0)
    }

    pub fn sys_link(&mut self, oldpath: *const u8, newpath: *const u8) -> SysResult {
        self.sys_linkat(AT_FDCWD, oldpath, AT_FDCWD, newpath, 0)
    }

    pub fn sys_linkat(
        &mut self,
        olddirfd: usize,
        oldpath: *const u8,
        newdirfd: usize,
        newpath: *const u8,
        flags: usize,
    ) -> SysResult {
        let proc = self.process();
        let oldpath = check_and_clone_cstr(oldpath)?;
        let newpath = check_and_clone_cstr(newpath)?;
        let flags = AtFlags::from_bits_truncate(flags);
        info!(
            "linkat: olddirfd: {}, oldpath: {:?}, newdirfd: {}, newpath: {:?}, flags: {:?}",
            olddirfd as isize, oldpath, newdirfd as isize, newpath, flags
        );

        let (new_dir_path, new_file_name) = split_path(&newpath);
        let inode = proc.lookup_inode_at(olddirfd, &oldpath, true)?;
        let new_dir_inode = proc.lookup_inode_at(newdirfd, new_dir_path, true)?;
        new_dir_inode.link(new_file_name, &inode)?;
        Ok(0)
    }

    pub fn sys_unlink(&mut self, path: *const u8) -> SysResult {
        self.sys_unlinkat(AT_FDCWD, path, 0)
    }

    pub fn sys_symlink(&mut self, target: *const u8, linkpath: *const u8) -> SysResult {
        self.sys_symlinkat(target, AT_FDCWD, linkpath)
    }

    pub fn sys_symlinkat(
        &mut self,
        target: *const u8,
        newdirfd: usize,
        linkpath: *const u8,
    ) -> SysResult {
        let proc = self.process();
        let target = check_and_clone_cstr(target)?;
        let linkpath = check_and_clone_cstr(linkpath)?;
        info!(
            "symlinkat: target: {} , newdirfd: {}, linkpath: {}",
            target, newdirfd as isize, linkpath,
        );
        let (dir_path, filename) = split_path(&linkpath);
        let dir_inode = proc.lookup_inode_at(newdirfd, dir_path, true)?;

        // If linkpath exists, it will not be overwritten.
        match dir_inode.find(filename) {
            Ok(_) => Err(SysError::EEXIST),
            Err(e) => match e {
                FsError::EntryNotFound => {
                    let symlink = dir_inode.create(filename, FileType::SymLink, 0o777)?;
                    symlink.write_at(0, target.as_bytes())?;
                    TimeSpec::update(&symlink);
                    TimeSpec::update(&dir_inode);
                    Ok(0)
                }
                _ => Err(e.into()),
            },
        }
    }

    pub fn sys_unlinkat(&mut self, dirfd: usize, path: *const u8, flags: usize) -> SysResult {
        let proc = self.process();
        let path = check_and_clone_cstr(path)?;
        let flags = AtFlags::from_bits_truncate(flags);
        info!(
            "unlinkat: dirfd: {}, path: {:?}, flags: {:?}",
            dirfd as isize, path, flags
        );

        let (dir_path, file_name) = split_path(&path);
        let dir_inode = proc.lookup_inode_at(dirfd, dir_path, true)?;
        let file_inode = dir_inode.find(file_name)?;
        if file_inode.metadata()?.type_ == FileType::Dir {
            return Err(SysError::EISDIR);
        }
        dir_inode.unlink(file_name)?;
        Ok(0)
    }

    pub fn sys_pipe(&mut self, fds: *mut u32) -> SysResult {
        self.sys_pipe2(fds, 0)
    }

    pub fn sys_pipe2(&mut self, fds: *mut u32, flags: usize) -> SysResult {
        info!("pipe2: fds: {:?}, flags: {:#x}", fds, flags);

        let mut proc = self.process();
        let fds = unsafe { self.vm().check_write_array(fds, 2)? };
        let (read, write) = Pipe::create_pair();

        let read_fd = proc.add_file(FileLike::File(FileHandle::new(
            Arc::new(read),
            OpenOptions {
                read: true,
                write: false,
                append: false,
                nonblock: (flags & O_NONBLOCK) != 0,
            },
            String::from("pipe_r:[]"),
            true,
            (flags & O_CLOEXEC) != 0,
        )));

        let write_fd = proc.add_file(FileLike::File(FileHandle::new(
            Arc::new(write),
            OpenOptions {
                read: false,
                write: true,
                append: false,
                nonblock: false,
            },
            String::from("pipe_w:[]"),
            true,
            (flags & O_CLOEXEC) != 0,
        )));

        fds[0] = read_fd as u32;
        fds[1] = write_fd as u32;

        info!("pipe: created rfd: {} wfd: {}", read_fd, write_fd);

        Ok(0)
    }

    pub fn sys_utimensat(
        &mut self,
        dirfd: usize,
        pathname: *const u8,
        times: *const TimeSpec,
        flags: usize,
    ) -> SysResult {
        info!(
            "utimensat(raw): dirfd: {}, pathname: {}, times: {}, flags: {:#x}",
            dirfd as i64, pathname as usize, times as usize, flags
        );
        const UTIME_NOW: usize = 0x3fffffff;
        const UTIME_OMIT: usize = 0x3ffffffe;
        let mut proc = self.process();
        let mut times = if times.is_null() {
            let epoch = TimeSpec::get_epoch();
            [epoch, epoch]
        } else {
            let times = unsafe { self.vm().check_read_array(times, 2)? };
            [times[0], times[1]]
        };
        let mut inode = if pathname.is_null() {
            let fd = dirfd;
            info!("futimens: fd: {}, times: {:?}", fd, times);
            proc.get_file(fd)?.inode()
        } else {
            let pathname = check_and_clone_cstr(pathname)?;
            info!(
                "utimensat: dirfd: {}, pathname: {}, times: {:?}, flags: {:#x}",
                dirfd as i64, pathname, times, flags
            );
            let follow = match flags {
                0 => true,
                fcntl::AT_SYMLINK_NOFOLLOW => false,
                _ => return Err(EINVAL),
            };
            proc.lookup_inode_at(dirfd, &pathname, follow)?
        };
        let mut metadata = inode.metadata()?;
        if times[0].nsec != UTIME_OMIT {
            if times[0].nsec == UTIME_NOW {
                times[0] = TimeSpec::get_epoch();
            }
            metadata.atime = rcore_fs::vfs::Timespec {
                sec: times[0].sec as i64,
                nsec: times[0].nsec as i32,
            };
        }
        if times[1].nsec != UTIME_OMIT {
            if times[1].nsec == UTIME_NOW {
                times[1] = TimeSpec::get_epoch();
            }
            metadata.mtime = rcore_fs::vfs::Timespec {
                sec: times[1].sec as i64,
                nsec: times[1].nsec as i32,
            };
        }
        inode.set_metadata(&metadata)?;
        Ok(0)
    }

    pub fn sys_sync(&mut self) -> SysResult {
        ROOT_INODE.fs().sync()?;
        Ok(0)
    }

    pub async fn sys_sendfile(
        &mut self,
        out_fd: usize,
        in_fd: usize,
        offset_ptr: UserInOutPtr<usize>,
        count: usize,
    ) -> SysResult {
        self.sys_copy_file_range(in_fd, offset_ptr, out_fd, UserInOutPtr::from(0), count, 0)
            .await
    }

    pub async fn sys_copy_file_range(
        &mut self,
        in_fd: usize,
        mut in_offset: UserInOutPtr<usize>,
        out_fd: usize,
        mut out_offset: UserInOutPtr<usize>,
        count: usize,
        flags: usize,
    ) -> SysResult {
        info!(
            "copy_file_range:BEG in: {}, out: {}, in_offset: {:?}, out_offset: {:?}, count: {} flags {}",
            in_fd, out_fd, in_offset, out_offset, count, flags
        );
        let proc = self.process();
        // We know it's save, pacify the borrow checker
        let proc_cell = UnsafeCell::new(proc);
        let in_file = unsafe { (*proc_cell.get()).get_file(in_fd)? };
        let out_file = unsafe { (*proc_cell.get()).get_file(out_fd)? };
        drop(proc_cell);
        let mut buffer = [0u8; 1024];

        // for in_offset and out_offset
        // null means update file offset
        // non-null means update {in,out}_offset instead

        let mut read_offset = if !in_offset.is_null() {
            in_offset.read()?
        } else {
            in_file.seek(SeekFrom::Current(0))? as usize
        };

        let orig_out_file_offset = out_file.seek(SeekFrom::Current(0))?;
        let write_offset = if !out_offset.is_null() {
            out_file.seek(SeekFrom::Start(out_offset.read()? as u64))? as usize
        } else {
            0
        };

        // read from specified offset and write new offset back
        let mut bytes_read = 0;
        let mut total_written = 0;
        while bytes_read < count {
            let len = min(buffer.len(), count - bytes_read);
            let read_len = in_file.read_at(read_offset, &mut buffer[..len]).await?;
            if read_len == 0 {
                break;
            }
            bytes_read += read_len;
            read_offset += read_len;

            let mut bytes_written = 0;
            let mut rlen = read_len;
            while bytes_written < read_len {
                let write_len = out_file.write(&buffer[bytes_written..(bytes_written + rlen)])?;
                if write_len == 0 {
                    info!(
                        "copy_file_range:END_ERR in: {}, out: {}, in_offset: {:?}, out_offset: {:?}, count: {} = bytes_read {}, bytes_written {}, write_len {}",
                        in_fd, out_fd, in_offset, out_offset, count, bytes_read, bytes_written, write_len
                    );
                    return Err(SysError::EBADF);
                }
                bytes_written += write_len;
                rlen -= write_len;
            }
            total_written += bytes_written;
        }

        if !in_offset.is_null() {
            in_offset.write(read_offset)?;
        } else {
            in_file.seek(SeekFrom::Current(bytes_read as i64))?;
        }

        if !out_offset.is_null() {
            out_offset.write(write_offset + total_written)?;
            out_file.seek(SeekFrom::Start(orig_out_file_offset))?;
        }
        info!(
            "copy_file_range:END in: {}, out: {}, in_offset: {:?}, out_offset: {:?}, count: {} flags {}",
            in_fd, out_fd, in_offset, out_offset, count, flags
        );
        return Ok(total_written);
    }

    pub fn sys_fcntl(&mut self, fd: usize, cmd: usize, arg: usize) -> SysResult {
        info!("fcntl: fd: {}, cmd: {:#x}, arg: {}", fd, cmd, arg);
        let mut proc = self.process();
        let file_like = proc.get_file_like(fd)?;
        match file_like {
            FileLike::File(file) => {
                use crate::fs::fcntl::*;
                match cmd {
                    F_SETFD => {
                        file.fd_cloexec = (arg & 1) != 0;
                        Ok(0)
                    }
                    F_GETFD => Ok(file.fd_cloexec as usize),
                    F_SETFL => {
                        file.set_options(arg);
                        Ok(0)
                    }
                    F_GETFL => self.unimplemented("F_GETFL", Ok(0)),
                    F_DUPFD_CLOEXEC => {
                        info!("fcntl: dupfd_cloexec: arg: {:#x}", arg);
                        // let file_like = proc.get_file_like(fd1)?.clone();
                        let new_fd = proc.get_free_fd_from(arg);
                        core::mem::drop(proc);
                        self.dup_impl(fd, new_fd, 1)
                    }
                    _ => Ok(0),
                }
            }
            FileLike::Socket(_) => {
                Ok(0)
                //TODO
            }
            FileLike::EpollInstance(_) => Ok(0),
        }
    }
}

impl Process {
    pub fn get_file_like(&mut self, fd: usize) -> Result<&mut FileLike, SysError> {
        self.files.get_mut(&fd).ok_or(SysError::EBADF)
    }
    pub fn get_file(&mut self, fd: usize) -> Result<&mut FileHandle, SysError> {
        match self.get_file_like(fd)? {
            FileLike::File(file) => Ok(file),
            _ => Err(SysError::EBADF),
        }
    }
    pub fn get_file_const(&self, fd: usize) -> Result<&FileHandle, SysError> {
        match self.files.get(&fd).ok_or(SysError::EBADF)? {
            FileLike::File(file) => Ok(file),
            _ => Err(SysError::EBADF),
        }
    }
    /// Lookup INode from the process.
    ///
    /// - If `path` is relative, then it is interpreted relative to the directory
    ///   referred to by the file descriptor `dirfd`.
    ///
    /// - If the `dirfd` is the special value `AT_FDCWD`, then the directory is
    ///   current working directory of the process.
    ///
    /// - If `path` is absolute, then `dirfd` is ignored.
    ///
    /// - If `follow` is true, then dereference `path` if it is a symbolic link.
    pub fn lookup_inode_at(
        &self,
        dirfd: usize,
        path: &str,
        follow: bool,
    ) -> Result<Arc<dyn INode>, SysError> {
        debug!(
            "lookup_inode_at: dirfd: {:?}, cwd: {:?}, path: {:?}, follow: {:?}",
            dirfd as isize, self.cwd, path, follow
        );
        // hard code special path
        match path {
            "/proc/self/exe" => {
                return Ok(Arc::new(Pseudo::new(&self.exec_path, FileType::SymLink)));
            }
            _ => {}
        }
        let (fd_dir_path, fd_name) = split_path(&path);
        match fd_dir_path {
            "/proc/self/fd" => {
                let fd: usize = fd_name.parse().map_err(|_| SysError::EINVAL)?;
                let fd_path = &self.get_file_const(fd)?.path;
                return Ok(Arc::new(Pseudo::new(fd_path, FileType::SymLink)));
            }
            _ => {}
        }

        let follow_max_depth = if follow { FOLLOW_MAX_DEPTH } else { 0 };
        if dirfd == AT_FDCWD {
            Ok(ROOT_INODE
                .lookup(&self.cwd)?
                .lookup_follow(path, follow_max_depth)?)
        } else {
            let file = match self.files.get(&dirfd).ok_or(SysError::EBADF)? {
                FileLike::File(file) => file,
                _ => return Err(SysError::EBADF),
            };
            Ok(file.lookup_follow(path, follow_max_depth)?)
        }
    }

    pub fn lookup_inode(&self, path: &str) -> Result<Arc<dyn INode>, SysError> {
        self.lookup_inode_at(AT_FDCWD, path, true)
    }
}

/// Split a `path` str to `(base_path, file_name)`
fn split_path(path: &str) -> (&str, &str) {
    let mut split = path.trim_end_matches('/').rsplitn(2, '/');
    let file_name = split.next().unwrap();
    let mut dir_path = split.next().unwrap_or(".");
    if dir_path == "" {
        dir_path = "/";
    }
    (dir_path, file_name)
}

impl From<FsError> for SysError {
    fn from(error: FsError) -> Self {
        match error {
            FsError::NotSupported => SysError::ENOSYS,
            FsError::NotFile => SysError::EISDIR,
            FsError::IsDir => SysError::EISDIR,
            FsError::NotDir => SysError::ENOTDIR,
            FsError::EntryNotFound => SysError::ENOENT,
            FsError::EntryExist => SysError::EEXIST,
            FsError::NotSameFs => SysError::EXDEV,
            FsError::InvalidParam => SysError::EINVAL,
            FsError::NoDeviceSpace => SysError::ENOMEM,
            FsError::DirRemoved => SysError::ENOENT,
            FsError::DirNotEmpty => SysError::ENOTEMPTY,
            FsError::WrongFs => SysError::EINVAL,
            FsError::DeviceError => SysError::EIO,
            FsError::IOCTLError => SysError::EINVAL,
            FsError::NoDevice => SysError::EINVAL,
            FsError::Again => SysError::EAGAIN,
            FsError::SymLoop => SysError::ELOOP,
            FsError::Busy => SysError::EBUSY,
            FsError::Interrupted => SysError::EINTR,
        }
    }
}

bitflags! {
    struct AtFlags: usize {
        const EMPTY_PATH = 0x1000;
        const SYMLINK_NOFOLLOW = 0x100;
    }
}

bitflags! {
    struct OpenFlags: usize {
        /// read only
        const RDONLY = 0;
        /// write only
        const WRONLY = 1;
        /// read write
        const RDWR = 2;
        /// create file if it does not exist
        const CREATE = 1 << 6;
        /// error if CREATE and the file exists
        const EXCLUSIVE = 1 << 7;
        /// truncate file upon open
        const TRUNCATE = 1 << 9;
        /// append on each write
        const APPEND = 1 << 10;
        /// close on exec
        const CLOEXEC = 1 << 19;
    }
}

impl OpenFlags {
    fn readable(&self) -> bool {
        let b = self.bits() & 0b11;
        b == OpenFlags::RDONLY.bits() || b == OpenFlags::RDWR.bits()
    }
    fn writable(&self) -> bool {
        let b = self.bits() & 0b11;
        b == OpenFlags::WRONLY.bits() || b == OpenFlags::RDWR.bits()
    }
    fn to_options(&self) -> OpenOptions {
        OpenOptions {
            read: self.readable(),
            write: self.writable(),
            append: self.contains(OpenFlags::APPEND),
            nonblock: false,
        }
    }
}

#[repr(packed)] // Don't use 'C'. Or its size will align up to 8 bytes.
pub struct LinuxDirent64 {
    /// Inode number
    ino: u64,
    /// Offset to next structure, an opaque value
    offset: u64,
    /// Size of this dirent
    reclen: u16,
    /// File type
    type_: u8,
    /// Filename (null-terminated)
    name: [u8; 0],
}

struct DirentBufWriter<'a> {
    buf: &'a mut [u8],
    ptr: *mut LinuxDirent64,
    rest_size: usize,
    written_size: usize,
}

impl<'a> DirentBufWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        DirentBufWriter {
            ptr: buf.as_mut_ptr() as *mut LinuxDirent64,
            rest_size: buf.len(),
            written_size: 0,
            buf,
        }
    }
    fn try_write(&mut self, inode: u64, type_: u8, name: &str) -> bool {
        let len = ::core::mem::size_of::<LinuxDirent64>() + name.len() + 1;
        let len = (len + 7) / 8 * 8; // align up
        if self.rest_size < len {
            return false;
        }
        let dent = LinuxDirent64 {
            ino: inode,
            offset: 0,
            reclen: len as u16,
            type_,
            name: [],
        };
        unsafe {
            self.ptr.write(dent);
            let name_ptr = self.ptr.add(1) as _;
            util::write_cstr(name_ptr, name);
            self.ptr = (self.ptr as *const u8).add(len) as _;
        }
        self.rest_size -= len;
        self.written_size += len;
        true
    }
}

bitflags! {
    pub struct DirentType: u8 {
        const DT_UNKNOWN  = 0;
        /// FIFO (named pipe)
        const DT_FIFO = 1;
        /// Character device
        const DT_CHR  = 2;
        /// Directory
        const DT_DIR  = 4;
        /// Block device
        const DT_BLK = 6;
        /// Regular file
        const DT_REG = 8;
        /// Symbolic link
        const DT_LNK = 10;
        /// UNIX domain socket
        const DT_SOCK  = 12;
        /// ???
        const DT_WHT = 14;
    }
}

impl DirentType {
    fn from_type(type_: &FileType) -> Self {
        match type_ {
            FileType::File => Self::DT_REG,
            FileType::Dir => Self::DT_DIR,
            FileType::SymLink => Self::DT_LNK,
            FileType::CharDevice => Self::DT_CHR,
            FileType::BlockDevice => Self::DT_BLK,
            FileType::Socket => Self::DT_SOCK,
            FileType::NamedPipe => Self::DT_FIFO,
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// ID of device containing file
    dev: u64,
    /// inode number
    ino: u64,
    /// number of hard links
    nlink: u64,

    /// file type and mode
    mode: StatMode,
    /// user ID of owner
    uid: u32,
    /// group ID of owner
    gid: u32,
    /// padding
    _pad0: u32,
    /// device ID (if special file)
    rdev: u64,
    /// total size, in bytes
    size: u64,
    /// blocksize for filesystem I/O
    blksize: u64,
    /// number of 512B blocks allocated
    blocks: u64,

    /// last access time
    atime: TimeSpec,
    /// last modification time
    mtime: TimeSpec,
    /// last status change time
    ctime: TimeSpec,
}

#[cfg(target_arch = "mips")]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Timespec {
    pub sec: i32,
    pub nsec: i32,
}

#[cfg(target_arch = "mips")]
#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// ID of device containing file
    dev: u64,
    /// padding
    __pad1: u64,
    /// inode number
    ino: u64,
    /// file type and mode
    mode: StatMode,
    /// number of hard links
    nlink: u32,

    /// user ID of owner
    uid: u32,
    /// group ID of owner
    gid: u32,
    /// device ID (if special file)
    rdev: u64,
    /// padding
    __pad2: u64,
    /// total size, in bytes
    size: u64,

    /// last access time
    atime: Timespec,
    /// last modification time
    mtime: Timespec,
    /// last status change time
    ctime: Timespec,

    /// blocksize for filesystem I/O
    blksize: u32,
    /// padding
    __pad3: u32,
    /// number of 512B blocks allocated
    blocks: u64,
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "mips")))]
#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// ID of device containing file
    dev: u64,
    /// inode number
    ino: u64,
    /// file type and mode
    mode: StatMode,
    /// number of hard links
    nlink: u32,

    /// user ID of owner
    uid: u32,
    /// group ID of owner
    gid: u32,
    /// device ID (if special file)
    rdev: u64,
    /// padding
    __pad: u64,
    /// total size, in bytes
    size: u64,
    /// blocksize for filesystem I/O
    blksize: u32,
    /// padding
    __pad2: u32,
    /// number of 512B blocks allocated
    blocks: u64,

    /// last access time
    atime: Timespec,
    /// last modification time
    mtime: Timespec,
    /// last status change time
    ctime: Timespec,
}

bitflags! {
    pub struct StatMode: u32 {
        const NULL  = 0;
        /// Type
        const TYPE_MASK = 0o170000;
        /// FIFO
        const FIFO  = 0o010000;
        /// character device
        const CHAR  = 0o020000;
        /// directory
        const DIR   = 0o040000;
        /// block device
        const BLOCK = 0o060000;
        /// ordinary regular file
        const FILE  = 0o100000;
        /// symbolic link
        const LINK  = 0o120000;
        /// socket
        const SOCKET = 0o140000;

        /// Set-user-ID on execution.
        const SET_UID = 0o4000;
        /// Set-group-ID on execution.
        const SET_GID = 0o2000;

        /// Read, write, execute/search by owner.
        const OWNER_MASK = 0o700;
        /// Read permission, owner.
        const OWNER_READ = 0o400;
        /// Write permission, owner.
        const OWNER_WRITE = 0o200;
        /// Execute/search permission, owner.
        const OWNER_EXEC = 0o100;

        /// Read, write, execute/search by group.
        const GROUP_MASK = 0o70;
        /// Read permission, group.
        const GROUP_READ = 0o40;
        /// Write permission, group.
        const GROUP_WRITE = 0o20;
        /// Execute/search permission, group.
        const GROUP_EXEC = 0o10;

        /// Read, write, execute/search by others.
        const OTHER_MASK = 0o7;
        /// Read permission, others.
        const OTHER_READ = 0o4;
        /// Write permission, others.
        const OTHER_WRITE = 0o2;
        /// Execute/search permission, others.
        const OTHER_EXEC = 0o1;
    }
}

impl StatMode {
    fn from_type_mode(type_: FileType, mode: u16) -> Self {
        let type_ = match type_ {
            FileType::File => StatMode::FILE,
            FileType::Dir => StatMode::DIR,
            FileType::SymLink => StatMode::LINK,
            FileType::CharDevice => StatMode::CHAR,
            FileType::BlockDevice => StatMode::BLOCK,
            FileType::Socket => StatMode::SOCKET,
            FileType::NamedPipe => StatMode::FIFO,
        };
        let mode = StatMode::from_bits_truncate(mode as u32);
        type_ | mode
    }
}

impl From<Metadata> for Stat {
    #[cfg(target_arch = "x86_64")]
    fn from(info: Metadata) -> Self {
        Stat {
            dev: info.dev as u64,
            ino: info.inode as u64,
            mode: StatMode::from_type_mode(info.type_, info.mode as u16),
            nlink: info.nlinks as u64,
            uid: info.uid as u32,
            gid: info.gid as u32,
            rdev: info.rdev as u64,
            size: info.size as u64,
            blksize: info.blk_size as u64,
            blocks: info.blocks as u64,
            atime: TimeSpec {
                sec: info.atime.sec as usize,
                nsec: info.atime.nsec as usize,
            },
            mtime: TimeSpec {
                sec: info.mtime.sec as usize,
                nsec: info.mtime.nsec as usize,
            },
            ctime: TimeSpec {
                sec: info.ctime.sec as usize,
                nsec: info.ctime.nsec as usize,
            },
            _pad0: 0,
        }
    }

    #[cfg(target_arch = "mips")]
    fn from(info: Metadata) -> Self {
        Stat {
            dev: info.dev as u64,
            ino: info.inode as u64,
            mode: StatMode::from_type_mode(info.type_, info.mode as u16),
            nlink: info.nlinks as u32,
            uid: info.uid as u32,
            gid: info.gid as u32,
            rdev: info.rdev as u64,
            size: info.size as u64,
            blksize: info.blk_size as u32,
            blocks: info.blocks as u64,
            atime: Timespec {
                sec: info.atime.sec as i32,
                nsec: info.atime.nsec,
            },
            mtime: Timespec {
                sec: info.mtime.sec as i32,
                nsec: info.mtime.nsec,
            },
            ctime: Timespec {
                sec: info.ctime.sec as i32,
                nsec: info.ctime.nsec,
            },
            __pad1: 0,
            __pad2: 0,
            __pad3: 0,
        }
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "mips")))]
    fn from(info: Metadata) -> Self {
        Stat {
            dev: info.dev as u64,
            ino: info.inode as u64,
            mode: StatMode::from_type_mode(info.type_, info.mode as u16),
            nlink: info.nlinks as u32,
            uid: info.uid as u32,
            gid: info.gid as u32,
            rdev: info.rdev as u64,
            size: info.size as u64,
            blksize: info.blk_size as u32,
            blocks: info.blocks as u64,
            atime: info.atime,
            mtime: info.mtime,
            ctime: info.ctime,
            __pad: 0,
            __pad2: 0,
        }
    }
}

const SEEK_SET: u8 = 0;
const SEEK_CUR: u8 = 1;
const SEEK_END: u8 = 2;

#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct IoVec {
    /// Starting address
    base: *mut u8,
    /// Number of bytes to transfer
    len: usize,
}

/// A valid IoVecs request from user
#[derive(Debug)]
pub struct IoVecs(Vec<&'static mut [u8]>);

impl IoVecs {
    pub unsafe fn check_and_new(
        iov_ptr: *const IoVec,
        iov_count: usize,
        vm: &MemorySet,
        readv: bool,
    ) -> Result<Self, SysError> {
        let iovs = vm.check_read_array(iov_ptr, iov_count)?.to_vec();
        let mut slices = vec![];
        slices.reserve(iovs.len());
        // check all bufs in iov
        for iov in iovs.iter() {
            // skip empty iov
            if iov.len == 0 {
                continue;
            }
            if readv {
                vm.check_write_array(iov.base, iov.len)?;
            } else {
                vm.check_read_array(iov.base, iov.len)?;
            }
            slices.push(slice::from_raw_parts_mut(iov.base, iov.len));
        }
        Ok(IoVecs(slices))
    }

    pub fn read_all_to_vec(&self) -> Vec<u8> {
        let mut buf = self.new_buf(false);
        for slice in self.0.iter() {
            buf.extend(slice.iter());
        }
        buf
    }

    pub fn write_all_from_slice(&mut self, buf: &[u8]) {
        let mut copied_len = 0;
        for slice in self.0.iter_mut() {
            let copy_len = min(slice.len(), buf.len() - copied_len);
            if copy_len == 0 {
                continue;
            }

            slice[..copy_len].copy_from_slice(&buf[copied_len..copied_len + copy_len]);
            copied_len += copy_len;
        }
    }

    /// Create a new Vec buffer from IoVecs
    /// For readv:  `set_len` is true,  Vec.len = total_len.
    /// For writev: `set_len` is false, Vec.cap = total_len.
    pub fn new_buf(&self, set_len: bool) -> Vec<u8> {
        let total_len = self.0.iter().map(|slice| slice.len()).sum::<usize>();
        let mut buf = Vec::with_capacity(total_len);
        if set_len {
            unsafe {
                buf.set_len(total_len);
            }
        }
        buf
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct PollFd {
    fd: u32,
    events: PollEvents,
    revents: PollEvents,
}

bitflags! {
    pub struct PollEvents: u16 {
        /// There is data to read.
        const IN = 0x0001;
        /// Writing is now possible.
        const OUT = 0x0004;
        /// Error condition (return only)
        const ERR = 0x0008;
        /// Hang up (return only)
        const HUP = 0x0010;
        /// Invalid request: fd not open (return only)
        const INVAL = 0x0020;
    }
}

const FD_PER_ITEM: usize = 8 * size_of::<u32>();
const MAX_FDSET_SIZE: usize = 1024 / FD_PER_ITEM;

struct FdSet {
    bitset: &'static mut BitSlice<Lsb0, u32>,
    origin: BitVec<Lsb0, u32>,
}

impl FdSet {
    /// Initialize a `FdSet` from pointer and number of fds
    /// Check if the array is large enough
    fn new(vm: &MemorySet, addr: *mut u32, nfds: usize) -> Result<FdSet, SysError> {
        if addr.is_null() {
            Ok(FdSet {
                bitset: BitSlice::empty_mut(),
                origin: BitVec::new(),
            })
        } else {
            let len = (nfds + FD_PER_ITEM - 1) / FD_PER_ITEM;
            if len > MAX_FDSET_SIZE {
                return Err(SysError::EINVAL);
            }
            let slice = unsafe { vm.check_write_array(addr, len)? };
            let bitset: &'static mut BitSlice<Lsb0, u32> = slice.into();
            debug!("bitset {:?}", bitset);

            // save the fdset, and clear it
            use alloc::borrow::ToOwned;
            let origin = bitset.to_owned();
            bitset.set_all(false);
            Ok(FdSet { bitset, origin })
        }
    }

    /// Try to set fd in `FdSet`
    /// Return true when `FdSet` is valid, and false when `FdSet` is bad (i.e. null pointer)
    /// Fd should be less than nfds
    fn set(&mut self, fd: usize) -> bool {
        if self.bitset.is_empty() {
            return false;
        }
        self.bitset.set(fd, true);
        true
    }

    /// Check to see whether `fd` is in original `FdSet`
    /// Fd should be less than nfds
    fn contains(&self, fd: usize) -> bool {
        if fd < self.bitset.len() {
            self.origin[fd]
        } else {
            false
        }
    }
}

/// Pathname is interpreted relative to the current working directory(CWD)
const AT_FDCWD: usize = -100isize as usize;
