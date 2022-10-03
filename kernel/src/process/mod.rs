pub use self::structs::*;
use crate::arch::cpu;
use crate::{
    consts::{MAX_CPU_NUM, MAX_PROCESS_NUM},
    memory::phys_to_virt,
    syscall::handle_syscall,
};
use alloc::{boxed::Box, sync::Arc};
use log::*;
use trapframe::UserContext;

mod abi;
pub mod futex;
pub mod proc;
pub mod structs;
pub mod thread;

use crate::sync::SpinNoIrqLock as Mutex;
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
pub use futex::*;
pub use proc::*;
pub use structs::*;
pub use thread::*;

pub fn init() {
    // create init process
    crate::shell::add_user_shell();

    info!("process: init end");
}

const PROCESSOR_INIT: Option<Arc<Thread>> = None;
static mut PROCESSORS: [Option<Arc<Thread>>; MAX_CPU_NUM] = [PROCESSOR_INIT; MAX_CPU_NUM];

/// Get current thread
///
/// `Thread` is a thread-local object.
/// It is safe to call this once, and pass `&mut Thread` as a function argument.
///
/// Don't use it unless necessary.
pub fn current_thread() -> Option<Arc<Thread>> {
    let cpu_id = cpu::id();
    unsafe { PROCESSORS[cpu_id].clone() }
}
