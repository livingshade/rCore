use super::{SysError::*, *};

use crate::bpf::consts::*;
use crate::bpf::map::*;
use crate::bpf::program::*;
use crate::bpf::tracepoints::*;

use alloc::vec::Vec;

impl Syscall<'_> {
    pub fn sys_bpf(&self, cmd: usize, attr_ptr: usize, _size: usize) -> SysResult {
        // error!("sys_bpf cmd = {}", cmd);
        match cmd {
            BPF_MAP_CREATE => {
                let ptr = UserInPtr::<MapAttr>::from(attr_ptr);
                let map_attr = ptr.read()?;
                // error!("map create: {:?}", map_attr);
                bpf_map_create(map_attr)
            }
            BPF_MAP_LOOKUP_ELEM | BPF_MAP_UPDATE_ELEM | BPF_MAP_DELETE_ELEM
            | BPF_MAP_GET_NEXT_KEY => {
                let ptr = UserInPtr::<MapOpAttr>::from(attr_ptr);
                let op_attr = ptr.read()?;
                let map_attr = bpf_map_get_attr(op_attr.map_fd).ok_or(ENOENT)?;
                self.handle_map_ops(cmd, op_attr, map_attr)
            }
            BPF_PROG_ATTACH => {
                let ptr = UserInPtr::<AttachTarget>::from(attr_ptr);
                let attach_attr = ptr.read()?;
                let target = check_and_clone_cstr(attach_attr.target)?;
                bpf_program_attach(&target, attach_attr.prog_fd)
            }
            // NOTE: non-standard command
            BPF_PROG_LOAD_EX => {
                let ptr = UserInPtr::<ProgramLoadExAttr>::from(attr_ptr);
                let attr = ptr.read()?;
                // ELF relocatable object info
                let base = attr.elf_prog as *mut u8;
                let size = attr.elf_size as usize;
                let prog = unsafe { self.vm().check_write_array(base, size)? };
                // map fd array
                let arr_len = attr.map_array_len as usize;
                let map_fd_array = unsafe { self.vm().check_read_array(attr.map_array, arr_len)? };
                let mut map_info = Vec::new();
                for i in 0..arr_len {
                    let entry = &map_fd_array[i];
                    let name = check_and_clone_cstr(entry.name)?;
                    map_info.push((name, entry.fd));
                }
                bpf_program_load_ex(prog, &map_info)
            }
            _ => Err(EINVAL),
        }
    }

    fn handle_map_ops(&self, op: usize, op_attr: MapOpAttr, map_attr: InternalMapAttr) -> SysResult {
        let vm = self.vm();
        // pointers
        let key = op_attr.key as *const u8;
        let value = op_attr.value as *mut u8;
        // sizes
        let key_sz = map_attr.key_size;
        let val_sz = map_attr.value_size;
        // we always need to read the key
        let _ = unsafe { vm.check_read_array(key, key_sz)? };
        match op {
            BPF_MAP_LOOKUP_ELEM => {
                let _ = unsafe { vm.check_write_array(value, val_sz)? };
            }
            BPF_MAP_UPDATE_ELEM => {
                let _ = unsafe { vm.check_read_array(value, val_sz)? };
            }
            BPF_MAP_GET_NEXT_KEY => {
                let _ = unsafe { vm.check_write_array(value, key_sz)? };
            }
            _ => {}
        }
        bpf_map_ops(op_attr.map_fd, op, key, value, op_attr.flags)
    }
}
