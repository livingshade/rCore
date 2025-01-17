use crate::consts::{PHYSICAL_MEMORY_OFFSET, KSEG2_START};
use crate::memory::{alloc_frame, dealloc_frame, phys_to_virt};
use core::mem::ManuallyDrop;
use log::*;
use rcore_memory::paging::*;
use riscv::addr::*;
use riscv::asm::{sfence_vma, sfence_vma_all};
use riscv::paging::MapperFlushable;
use riscv::paging::PTE;
use riscv::paging::{FrameAllocatorFor, FrameDeallocatorFor};
use riscv::paging::{Mapper, PageTable as RvPageTable, PageTableEntry, PageTableFlags as EF};
use riscv::register::satp;

#[cfg(target_arch = "riscv32")]
type TopLevelPageTable<'a> = riscv::paging::Rv32PageTable<'a>;
#[cfg(target_arch = "riscv64")]
type TopLevelPageTable<'a> = riscv::paging::Rv39PageTable<'a>;

use riscv::use_sv32;
use riscv::use_sv39;
#[cfg(target_arch = "riscv32")]
use_sv32!();
#[cfg(target_arch = "riscv64")]
use_sv39!();

pub struct PageTableImpl {
    page_table: TopLevelPageTable<'static>,
    root_frame: Frame,
    entry: Option<PageEntry>,
}

/// PageTableEntry: the contents of this entry.
/// Page: this entry is the pte of page `Page`.
pub struct PageEntry(&'static mut PageTableEntry, Page);

impl PageTable for PageTableImpl {
    fn map(&mut self, addr: usize, target: usize) -> &mut dyn Entry {
        // map the 4K `page` to the 4K `frame` with `flags`
        let flags = EF::VALID | EF::READABLE | EF::WRITABLE;
        let page = Page::of_addr(VirtAddr::new(addr));
        let frame = Frame::of_addr(PhysAddr::new_u64(target as u64));
        // we may need frame allocator to alloc frame for new page table(first/second)
        self.page_table
            .map_to(page, frame, flags, &mut FrameAllocatorForRiscv)
            .unwrap()
            .flush();
        self.get_entry(addr).expect("fail to get entry")
    }

    fn unmap(&mut self, addr: usize) {
        let page = Page::of_addr(VirtAddr::new(addr));
        let (_, flush) = self.page_table.unmap(page).unwrap();
        flush.flush();
    }

    fn get_entry(&mut self, vaddr: usize) -> Option<&mut dyn Entry> {
        let page = Page::of_addr(VirtAddr::new(vaddr));
        if let Ok(e) = self.page_table.ref_entry(page.clone()) {
            let e = unsafe { &mut *(e as *mut PageTableEntry) };
            self.entry = Some(PageEntry(e, page));
            Some(self.entry.as_mut().unwrap())
        } else {
            None
        }
    }

    fn get_page_slice_mut<'a>(&mut self, addr: usize) -> &'a mut [u8] {
        let frame = self
            .page_table
            .translate_page(Page::of_addr(VirtAddr::new(addr)))
            .unwrap();
        let vaddr = frame.start_address().as_usize() + PHYSICAL_MEMORY_OFFSET;
        unsafe { core::slice::from_raw_parts_mut(vaddr as *mut u8, 0x1000) }
    }

    fn flush_cache_copy_user(&mut self, _start: usize, _end: usize, _execute: bool) {}
}

/// implementation for the Entry trait in /crate/memory/src/paging/mod.rs
impl Entry for PageEntry {
    fn update(&mut self) {
        unsafe {
            sfence_vma(0, self.1.start_address().as_usize());
        }
    }
    fn accessed(&self) -> bool {
        self.0.flags().contains(EF::ACCESSED)
    }
    fn dirty(&self) -> bool {
        self.0.flags().contains(EF::DIRTY)
    }
    fn writable(&self) -> bool {
        self.0.flags().contains(EF::WRITABLE)
    }
    fn present(&self) -> bool {
        self.0.flags().contains(EF::VALID | EF::READABLE)
    }
    fn clear_accessed(&mut self) {
        self.0.flags_mut().remove(EF::ACCESSED);
    }
    fn clear_dirty(&mut self) {
        self.0.flags_mut().remove(EF::DIRTY);
    }
    fn set_writable(&mut self, value: bool) {
        self.0.flags_mut().set(EF::WRITABLE, value);
    }
    fn set_present(&mut self, value: bool) {
        self.0.flags_mut().set(EF::VALID | EF::READABLE, value);
    }
    fn target(&self) -> usize {
        self.0.addr::<PhysAddr>().as_usize()
    }
    fn set_target(&mut self, target: usize) {
        let flags = self.0.flags();
        let frame = Frame::of_addr(PhysAddr::new_u64(target as u64));
        self.0.set(frame, flags);
    }
    fn writable_shared(&self) -> bool {
        self.0.flags().contains(EF::RESERVED1)
    }
    fn readonly_shared(&self) -> bool {
        self.0.flags().contains(EF::RESERVED2)
    }
    fn set_shared(&mut self, writable: bool) {
        let flags = self.0.flags_mut();
        flags.set(EF::RESERVED1, writable);
        flags.set(EF::RESERVED2, !writable);
    }
    fn clear_shared(&mut self) {
        self.0.flags_mut().remove(EF::RESERVED1 | EF::RESERVED2);
    }
    fn swapped(&self) -> bool {
        self.0.flags().contains(EF::RESERVED1)
    }
    fn set_swapped(&mut self, value: bool) {
        self.0.flags_mut().set(EF::RESERVED1, value);
    }
    fn user(&self) -> bool {
        self.0.flags().contains(EF::USER)
    }
    fn set_user(&mut self, value: bool) {
        self.0.flags_mut().set(EF::USER, value);
    }
    fn execute(&self) -> bool {
        self.0.flags().contains(EF::EXECUTABLE)
    }
    fn set_execute(&mut self, value: bool) {
        self.0.flags_mut().set(EF::EXECUTABLE, value);
    }
    fn mmio(&self) -> u8 {
        0
    }
    fn set_mmio(&mut self, _value: u8) {}
}

#[cfg(target_arch = "riscv32")]
const TOKEN_MASK: usize = 0x7fffffff;
#[cfg(target_arch = "riscv64")]
const TOKEN_MASK: usize = 0x0fffffff_ffffffff;

fn get_kernel_page_table_frame() -> Frame {
    let token = unsafe { super::memory::SATP };
    Frame::of_ppn(token & TOKEN_MASK)
}

impl PageTableImpl {
    /// Unsafely get the current active page table.
    /// Using ManuallyDrop to wrap the page table: this is how `core::mem::forget` is implemented now.
    pub unsafe fn active() -> ManuallyDrop<Self> {
        let frame = Frame::of_ppn(PageTableImpl::active_token() & TOKEN_MASK);
        let table = frame.as_kernel_mut(PHYSICAL_MEMORY_OFFSET as u64);
        ManuallyDrop::new(PageTableImpl {
            page_table: TopLevelPageTable::new(table, PHYSICAL_MEMORY_OFFSET),
            root_frame: frame,
            entry: None,
        })
    }
    /// The method for getting the kernel page table.
    /// Returns the *only* global kernel page table.
    pub unsafe fn kernel_table() -> ManuallyDrop<Self> {
        let frame = get_kernel_page_table_frame();
        let table = frame.as_kernel_mut(PHYSICAL_MEMORY_OFFSET as u64);
        ManuallyDrop::new(PageTableImpl {
            page_table: TopLevelPageTable::new(table, PHYSICAL_MEMORY_OFFSET),
            root_frame: frame,
            entry: None,
        })        
    }

    /// The method for mapping kernel pages during initialization.
    pub fn map_kernel_initial(&mut self) {
        info!("mapping kernel linear mapping");
        let table = unsafe {
            &mut *(phys_to_virt(self.root_frame.start_address().as_usize()) as *mut RvPageTable)
        };
        #[cfg(target_arch = "riscv32")]
        for i in 256..1024 {
            let flags =
                EF::VALID | EF::READABLE | EF::WRITABLE | EF::EXECUTABLE | EF::ACCESSED | EF::DIRTY | EF::GLOBAL;
            let frame = Frame::of_addr(PhysAddr::new((i << 22) - PHYSICAL_MEMORY_OFFSET));
            table[i].set(frame, flags);
        }
        #[cfg(target_arch = "riscv64")]
        for i in 509..512 {
            if i == 510 {
                // MMIO range 0x60000000 - 0x7FFFFFFF does not work as a large page, dunno why
                continue;
            }
            let flags =
                EF::VALID | EF::READABLE | EF::WRITABLE | EF::EXECUTABLE | EF::ACCESSED | EF::DIRTY | EF::GLOBAL;
            let frame = Frame::of_addr(PhysAddr::new_u64(
                ((0xFFFFFF80_00000000 + (i << 30)) - PHYSICAL_MEMORY_OFFSET) as u64,
            ));
            table[i].set(frame, flags);
        }
    }
}

impl PageTableExt for PageTableImpl {
    fn new_bare() -> Self {
        let target = alloc_frame().expect("failed to allocate frame");
        let frame = Frame::of_addr(PhysAddr::new_u64(target as u64));

        let table = unsafe { &mut *(phys_to_virt(target) as *mut RvPageTable) };
        table.zero();

        PageTableImpl {
            page_table: TopLevelPageTable::new(table, PHYSICAL_MEMORY_OFFSET),
            root_frame: frame,
            entry: None,
        }
    }

    fn map_kernel(&mut self) {
        let kernel_table_frame = get_kernel_page_table_frame();
        let kernel_table = unsafe {
            &mut *(phys_to_virt(kernel_table_frame.start_address().as_usize()) as *mut RvPageTable)
        };
        let table = unsafe {
            &mut *(phys_to_virt(self.root_frame.start_address().as_usize()) as *mut RvPageTable)
        };

        #[cfg(target_arch = "riscv32")]
        for i in 256..1024 {
            table[i].set(kernel_table[i].frame::<PhysAddrSv32>(), kernel_table[i].flags());
        }
        #[cfg(target_arch = "riscv64")]
        {
            for i in 509..512 {
                table[i].set(kernel_table[i].frame::<PhysAddrSv39>(), kernel_table[i].flags());
            }
            // extra mapping for KSEG2 (used for kernel modules)
            let i = (KSEG2_START - 0xFFFFFF80_00000000) >> 30;
            table[i].set(kernel_table[i].frame::<PhysAddrSv39>(), kernel_table[i].flags());
        }
    }

    fn token(&self) -> usize {
        #[cfg(target_arch = "riscv32")]
        return self.root_frame.number() | (1 << 31);
        #[cfg(target_arch = "riscv64")]
        return self.root_frame.number() | (8 << 60);
    }

    unsafe fn set_token(token: usize) {
        satp::write(token);
    }

    fn active_token() -> usize {
        satp::read().bits()
    }

    fn flush_tlb() {
        unsafe {
            sfence_vma_all();
        }
    }
}

impl Drop for PageTableImpl {
    fn drop(&mut self) {
        dealloc_frame(self.root_frame.start_address().as_usize());
    }
}

struct FrameAllocatorForRiscv;

impl FrameAllocatorFor<PhysAddr> for FrameAllocatorForRiscv {
    fn alloc(&mut self) -> Option<Frame> {
        alloc_frame().map(|addr| Frame::of_addr(PhysAddr::new_u64(addr as u64)))
    }
}

impl FrameDeallocatorFor<PhysAddr> for FrameAllocatorForRiscv {
    fn dealloc(&mut self, frame: Frame) {
        dealloc_frame(frame.start_address().as_usize());
    }
}
