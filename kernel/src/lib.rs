#![feature(lang_items)]
#![feature(naked_functions)]
#![feature(panic_info_message)]
#![feature(negative_impls)]
#![feature(map_first_last)]
#![deny(unused_must_use)]
#![deny(stable_features)]
#![deny(unused_unsafe)]
#![deny(ellipsis_inclusive_range_patterns)]
#![deny(unused_parens)]
#![deny(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(dead_code)]
#![allow(unused_mut)]
#![allow(unused_variables)]
#![allow(unused_imports)]
#![no_std]

// just keep it ...
#[macro_use]
extern crate alloc;
#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;
extern crate num;
extern crate rlibc;
#[macro_use]
extern crate num_derive;

pub use buddy_system_allocator::LockedHeapWithRescue;

#[macro_use] // print!
pub mod logging;
#[macro_use]
pub mod util;

pub mod backtrace;
pub mod consts;
pub mod drivers;
pub mod fs;
pub mod ipc;
pub mod lang;
pub mod lkm;
pub mod memory;
pub mod net;
pub mod process;
#[cfg(feature = "hypervisor")]
pub mod rvm;
pub mod shell;
pub mod signal;
pub mod sync;
pub mod syscall;
pub mod trap;

#[allow(dead_code)]
#[cfg(target_arch = "x86_64")]
#[path = "arch/x86_64/mod.rs"]
pub mod arch;

#[cfg(target_arch = "mips")]
#[path = "arch/mipsel/mod.rs"]
pub mod arch;

#[cfg(riscv)]
#[path = "arch/riscv/mod.rs"]
pub mod arch;

#[cfg(target_arch = "aarch64")]
#[path = "arch/aarch64/mod.rs"]
pub mod arch;

pub mod bpf;
pub mod kprobes;

pub fn kmain() -> ! {
    // kprobes::kprobes::run_kprobes_tests();
    // kprobes::kretprobes::run_kretprobes_test();
    if arch::cpu::id() == 0 {
        kprobes::trace::run_dynamic_trace_test();
    }

    loop {
        executor::run_until_idle();
        arch::interrupt::wait_for_interrupt();
    }
}

/// Global heap allocator
///
/// Available after `memory::init()`.
///
/// It should be defined in memory mod, but in Rust `global_allocator` must be in root mod.
#[global_allocator]
static HEAP_ALLOCATOR: LockedHeapWithRescue<32> =
    LockedHeapWithRescue::new(crate::memory::enlarge_heap);
