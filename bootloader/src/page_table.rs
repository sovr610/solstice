use crate::frame_allocator::FrameAllocator;
use bootloader::bootinfo::MemoryRegionType;
use fixedvec::FixedVec;
use x86_64::{
    align_up,
    structures::paging::{
        self,
        mapper::{MapToError, Mapper, MapperFlush, UnmapError},
        page::Size4KiB,
        Page,
        PageSize,
        PageTableFlags,
        PhysFrame,
        RecursivePageTable,
    },
    PhysAddr,
    VirtAddr,
};
use xmas_elf::program::{self, ProgramHeader64};

pub(crate) fn map_kernel(
    kernel_start: PhysAddr,
    segments: &FixedVec<ProgramHeader64>,
    page_table: &mut RecursivePageTable,
    frame_allocator: &mut FrameAllocator,
) -> Result<VirtAddr, MapToError> {
    for segment in segments {
        map_segment(segment, kernel_start, page_table, frame_allocator)?;
    }

    /*extern "C" {
        static kernel_stack_top: usize;
        static kernel_stack_guard: usize;
    };

    map_page(
        Page::<Size4KiB>::containing_address(VirtAddr::new(
            unsafe { &kernel_stack_guard as *const _ as usize } + crate::PHYSICAL_MEMORY_OFFSET,
        )),
        PhysFrame::containing_address(PhysAddr::new(unsafe {
            &kernel_stack_guard as *const _ as usize
        })),
        PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE,
        page_table,
        frame_allocator,
    );

    Ok(VirtAddr::new(
        unsafe { &kernel_stack_top as *const _ as usize } + crate::PHYSICAL_MEMORY_OFFSET,
    ))*/

    let stack_start = Page::containing_address(VirtAddr::new(0x57AC_0000_0000));
    let stack_size = 128; // in pages
    let stack_end = stack_start + stack_size;

    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    let region_type = MemoryRegionType::KernelStack;

    for page in Page::range(stack_start, stack_end) {
        let frame = frame_allocator
            .allocate_frame(region_type)
            .ok_or(MapToError::FrameAllocationFailed)?;
        map_page(page, frame, flags, page_table, frame_allocator)?.flush();
    }

    Ok(stack_end.start_address())
}

pub(crate) fn map_segment(
    segment: &ProgramHeader64,
    kernel_start: PhysAddr,
    page_table: &mut RecursivePageTable,
    frame_allocator: &mut FrameAllocator,
) -> Result<(), MapToError> {
    let typ = segment.get_type().unwrap();
    match typ {
        program::Type::Load => {
            let mem_size = segment.mem_size as usize;
            let file_size = segment.file_size as usize;
            let file_offset = segment.offset as usize;
            let phys_start_addr = kernel_start + file_offset as usize;
            let virt_start_addr = VirtAddr::new(segment.virtual_addr as usize);

            let start_page: Page = Page::containing_address(virt_start_addr);
            let start_frame = PhysFrame::containing_address(phys_start_addr);
            let end_frame =
                PhysFrame::containing_address(phys_start_addr + file_size as usize - 1usize);

            let flags = segment.flags;
            let mut page_table_flags = PageTableFlags::PRESENT;
            if !flags.is_execute() {
                page_table_flags |= PageTableFlags::NO_EXECUTE
            };
            if flags.is_write() {
                page_table_flags |= PageTableFlags::WRITABLE
            };

            for frame in PhysFrame::range_inclusive(start_frame, end_frame) {
                let offset = frame - start_frame;
                let page = start_page + offset;
                map_page(page, frame, page_table_flags, page_table, frame_allocator)?.flush();
            }

            if mem_size > file_size {
                // .bss section (or similar), which needs to be zeroed
                let zero_start = virt_start_addr + file_size as usize;
                let zero_end = virt_start_addr + mem_size as usize;
                if zero_start.as_usize() & 0xfff != 0 {
                    // A part of the last mapped frame needs to be zeroed. This is
                    // not possible since it could already contains parts of the next
                    // segment. Thus, we need to copy it before zeroing.

                    // TODO: search for a free page dynamically
                    let temp_page: Page = Page::containing_address(VirtAddr::new(0xfeeefeee000));
                    let new_frame = frame_allocator
                        .allocate_frame(MemoryRegionType::Kernel)
                        .ok_or(MapToError::FrameAllocationFailed)?;
                    map_page(
                        temp_page.clone(),
                        new_frame.clone(),
                        page_table_flags,
                        page_table,
                        frame_allocator,
                    )?
                    .flush();

                    type PageArray = [usize; Size4KiB::SIZE as usize / 8];

                    let last_page =
                        Page::containing_address(virt_start_addr + file_size as usize - 1usize);
                    let last_page_ptr = last_page.start_address().as_ptr::<PageArray>();
                    let temp_page_ptr = temp_page.start_address().as_mut_ptr::<PageArray>();

                    unsafe {
                        // copy contents
                        temp_page_ptr.write(last_page_ptr.read());
                    }

                    // remap last page
                    if let Err(e) = page_table.unmap(last_page.clone()) {
                        return Err(match e {
                            UnmapError::ParentEntryHugePage => MapToError::ParentEntryHugePage,
                            UnmapError::PageNotMapped => unreachable!(),
                            UnmapError::InvalidFrameAddress(_) => unreachable!(),
                        });
                    }

                    map_page(
                        last_page,
                        new_frame,
                        page_table_flags,
                        page_table,
                        frame_allocator,
                    )?
                    .flush();
                }

                // Map additional frames.
                let start_page: Page = Page::containing_address(VirtAddr::new(align_up(
                    zero_start.as_usize(),
                    Size4KiB::SIZE,
                )));
                let end_page = Page::containing_address(zero_end);
                for page in Page::range_inclusive(start_page, end_page) {
                    let frame = frame_allocator
                        .allocate_frame(MemoryRegionType::Kernel)
                        .ok_or(MapToError::FrameAllocationFailed)?;
                    map_page(page, frame, page_table_flags, page_table, frame_allocator)?.flush();
                }

                // zero
                for offset in file_size..mem_size {
                    let addr = virt_start_addr + offset as usize;
                    unsafe { addr.as_mut_ptr::<u8>().write(0) };
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn map_page<'a, S>(
    page: Page<S>,
    phys_frame: PhysFrame<S>,
    flags: PageTableFlags,
    page_table: &mut RecursivePageTable<'a>,
    frame_allocator: &mut FrameAllocator,
) -> Result<MapperFlush<S>, MapToError>
where
    S: PageSize,
    RecursivePageTable<'a>: Mapper<S>,
{
    struct PageTableAllocator<'a, 'b: 'a>(&'a mut FrameAllocator<'b>);

    unsafe impl<'a, 'b> paging::FrameAllocator<Size4KiB> for PageTableAllocator<'a, 'b> {
        fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
            self.0.allocate_frame(MemoryRegionType::PageTable)
        }
    }

    unsafe {
        page_table.map_to(
            page,
            phys_frame,
            flags,
            &mut PageTableAllocator(frame_allocator),
        )
    }
}
