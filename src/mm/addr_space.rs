use crate::{ds::RwSpinLock, mm::pmm::PhysAllocator};
use x86_64::{
    registers::control::Cr3,
    structures::paging::{
        frame::PhysFrame,
        mapper::{MapToError, MapperAllSizes, MapperFlush},
        page::Size4KiB,
        FrameAllocator,
        Mapper,
        OffsetPageTable,
        Page,
        PageTableFlags,
    },
    PhysAddr,
    VirtAddr,
};

pub struct AddrSpace {
    table: RwSpinLock<OffsetPageTable<'static>>,
}

unsafe impl Send for AddrSpace {}
unsafe impl Sync for AddrSpace {}

lazy_static! {
    static ref KERNEL: AddrSpace = {
        let (table_frame, _) = Cr3::read();
        let table_virt: VirtAddr = table_frame.start_address().into();

        AddrSpace {
            table: RwSpinLock::new(unsafe {
                OffsetPageTable::new(&mut *table_virt.as_mut_ptr(), super::PHYS_OFFSET)
            }),
        }
    };
}

impl AddrSpace {
    pub fn kernel() -> &'static AddrSpace {
        &*KERNEL
    }

    pub fn map_to(
        &self,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageTableFlags,
    ) -> Result<MapperFlush<Size4KiB>, MapToError> {
        struct PhysAllocatorProxy;
        unsafe impl FrameAllocator<Size4KiB> for PhysAllocatorProxy {
            fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
                Some(PhysAllocator::alloc(0).start)
            }
        }

        self.map_to_with_allocator(virt, phys, flags, &mut PhysAllocatorProxy)
    }

    // TODO: Make sure that allocations and deallocations are done with the same
    // allocator?
    pub fn map_to_with_allocator<A: FrameAllocator<Size4KiB>>(
        &self,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageTableFlags,
        alloc: &mut A,
    ) -> Result<MapperFlush<Size4KiB>, MapToError> {
        unsafe {
            self.table.write().map_to(
                Page::containing_address(virt),
                PhysFrame::containing_address(phys),
                flags,
                alloc,
            )
        }
    }

    pub fn translate_addr(&self, addr: VirtAddr) -> Option<PhysAddr> {
        self.table.read().translate_addr(addr)
    }
}
