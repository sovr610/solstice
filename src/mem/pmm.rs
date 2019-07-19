use crate::mem::map::{MemoryMap, Region, RegionBumpAllocator};
use arrayvec::ArrayVec;
use core::{alloc::Layout, mem, num::NonZeroU8, slice};
use x86_64::VirtAddr;

pub const MAX_ZONES: usize = 64;
pub const MAX_ORDER: usize = 11;
pub const MAX_ORDER_PAGES: usize = 1 << 11;

#[derive(Debug)]
struct Zone {
    addr: VirtAddr,
    num_pages: usize,
    order_list: [&'static mut [Block]; MAX_ORDER + 1],
}

impl Zone {
    pub fn new(addr: VirtAddr, size: usize, blocks: &'static mut [Block]) -> Self {
        let num_pages = size / super::PAGE_SIZE;

        let mut order_list = Self::split_region(num_pages, blocks);

        let mut blocks_in_order = num_pages;
        for (order, list) in order_list.iter_mut().enumerate() {
            for block in list.iter_mut().take(blocks_in_order) {
                *block = Block::order(order as u8);
            }

            blocks_in_order /= 2;
        }

        let largest_order =
            (num_pages.next_power_of_two().trailing_zeros() as usize).min(MAX_ORDER + 1);
        for list in order_list[largest_order..].iter_mut() {
            list[0] = Block::order(largest_order as u8 - 1);
        }

        Zone {
            addr,
            num_pages,
            order_list,
        }
    }

    fn split_region(
        num_pages: usize,
        mut blocks: &'static mut [Block],
    ) -> [&'static mut [Block]; MAX_ORDER + 1] {
        let max_order_blocks = x86_64::align_up(num_pages, MAX_ORDER_PAGES) / MAX_ORDER_PAGES;

        // TODO: This whole section is a bit of a hack
        let mut tmp: [Option<&'static mut [Block]>; MAX_ORDER + 1] = [
            None, None, None, None, None, None, None, None, None, None, None, None,
        ];

        for (order, block_slice) in tmp.iter_mut().rev().enumerate() {
            let blocks_in_layer = max_order_blocks * 2_usize.pow(order as u32);
            let (left, right) = blocks.split_at_mut(blocks_in_layer);
            *block_slice = Some(left);
            blocks = right;
        }

        unsafe { core::mem::transmute(tmp) }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Block {
    LargestFreeOrder(NonZeroU8),
    Used,
}

impl core::fmt::Debug for Block {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Block::LargestFreeOrder(nzu) => {
                fmt.write_fmt(format_args!("LargestFreeOrder({})", nzu.get() - 1))
            }
            Block::Used => fmt.write_str("Used"),
        }
    }
}

impl Block {
    fn order(largest_free_order: u8) -> Self {
        Block::LargestFreeOrder(unsafe { NonZeroU8::new_unchecked(largest_free_order + 1) })
    }

    fn new_blocks_for_region(region: Region, usable_pages: usize) -> &'static mut [Block] {
        let block_count = blocks_in_region(usable_pages);

        let mut rg_allocator = RegionBumpAllocator::from(region);
        let ptr = rg_allocator
            .alloc(
                Layout::from_size_align(
                    block_count * mem::size_of::<Block>(),
                    mem::align_of::<Block>(),
                )
                .unwrap(),
            )
            .expect("failed to allocate from region");

        debug_assert_eq!(
            ptr.as_ptr() as usize,
            x86_64::align_down(ptr.as_ptr() as usize, super::PAGE_SIZE)
        );

        unsafe {
            // Zero out the memory, which corresponds to Block::Used
            core::intrinsics::write_bytes(ptr.as_ptr(), 0, block_count);
            slice::from_raw_parts_mut(ptr.as_ptr() as *mut Block, block_count)
        }
    }
}

struct PageInfo;

pub struct PhysAllocator {
    zones: ArrayVec<[Zone; MAX_ZONES]>,
}

impl PhysAllocator {
    pub fn new(map: MemoryMap) -> Self {
        let mut zones = ArrayVec::new();

        for rg in map {
            let pages_in_rg = rg.size / super::PAGE_SIZE;
            let usable_pages = usable_pages(pages_in_rg);
            if usable_pages <= 1 {
                continue;
            }

            let (reserved, usable) = rg.split_at((pages_in_rg - usable_pages) * super::PAGE_SIZE);

            zones.push(Zone::new(
                usable.addr.into(),
                x86_64::align_down(usable.size, super::PAGE_SIZE),
                Block::new_blocks_for_region(reserved, usable_pages),
            ));

            assert_eq!(usable.addr.as_usize() & (super::PAGE_SIZE - 1), 0); // Make sure it's aligned
        }

        Self { zones }
    }
}

// Each page of memory has a constant memory overhead of size_of::<PageInfo>(),
// as well as the whole region having a memory overhead of
// blocks_in_region() * size_of::<Block>().
// Let N = number of (PMM) usable memory pages
//     T = total number of pages, usable and unusable
//     W = overhead per page in bytes
// We have the equation
//       total wasted bytes <= 4096 * (T - N)
// N * W + blocks_in_region <= 4096T - 4096N
//           N * (W + 4096) <= 4096T - blocks_in_region
//                    N - 1 < (4096T - blocks_in_region) / (W + 4096)
// Hence: Max usable N = 4096T / (W + 4096) - 1
// Subtract one extra page, just to be safe about padding and alignment
// TODO: should really be blocks_in_region(usable_pages), but this hugely
// complicates the math
fn usable_pages(total_pages: usize) -> usize {
    (4096 * total_pages as usize - blocks_in_region(total_pages))
        / (mem::size_of::<PageInfo>() + 4096)
        - 2
}

fn blocks_in_region(pages: usize) -> usize {
    let max_order_blocks = x86_64::align_up(pages, MAX_ORDER_PAGES) / MAX_ORDER_PAGES;
    // Evaluate the geometric series
    // a = max_order_blocks
    // r = 2
    // n = max_order + 1
    max_order_blocks * (2usize.pow(MAX_ORDER as u32 + 1) - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    test_case!(block_repr, {
        assert_eq!(mem::size_of::<Block>(), 1);
        assert_eq!(mem::align_of::<Block>(), 1);

        // Check that 0 corresponds to Block::Used
        let b: u8 = 0;
        let block = &b as *const u8 as *const Block;
        assert_eq!(unsafe { *block }, Block::Used);
    });
}
