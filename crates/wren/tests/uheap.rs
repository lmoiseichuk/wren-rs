//! The fixed-size heap, which has to be right rather than fast.
//!
//! Every test here is about an invariant, because the failures this design
//! can have are the quiet kind: two blocks overlapping, a hole that stops
//! being reusable, a table that grows into the blocks it describes.

use wren::uheap::{UHeap, Unit};

fn buffer(units: usize) -> Vec<Unit> {
    // Deliberately not zeroed, and filled with something recognisable: the
    // heap must never read a unit it has not written.
    vec![0xdead_beef_usize; units]
}

#[test]
fn a_block_comes_back_and_the_heap_knows_how_much_is_gone() {
    let mut memory = buffer(64);
    let mut heap = UHeap::new(&mut memory);
    assert_eq!(heap.blocks(), 0);
    assert_eq!(heap.used(), 0);

    let a = heap.alloc(4).expect("fits");
    assert_eq!(heap.blocks(), 1);
    // Four units of block and one of record.
    assert_eq!(heap.used(), 5);

    assert!(heap.free(a));
    // The last block freed is not a hole; it goes back to the boundary.
    assert_eq!(heap.blocks(), 0);
    assert_eq!(heap.used(), 0);
}

#[test]
fn blocks_are_cut_downwards_and_never_overlap() {
    let mut memory = buffer(64);
    let mut heap = UHeap::new(&mut memory);
    let a = heap.alloc(4).expect("fits");
    let b = heap.alloc(8).expect("fits");
    let c = heap.alloc(2).expect("fits");

    assert_eq!(a, 60, "the first block sits at the very end");
    assert_eq!(b, 52, "the next is below it");
    assert_eq!(c, 50);
    assert!(b + 8 == a && c + 2 == b, "blocks are adjacent with no gap");
}

#[test]
fn writing_a_block_does_not_touch_its_neighbours() {
    let mut memory = buffer(32);
    let mut heap = UHeap::new(&mut memory);
    let a = heap.alloc(3).expect("fits");
    let b = heap.alloc(3).expect("fits");
    heap.block(a, 3).fill(0xaaaa);
    heap.block(b, 3).fill(0xbbbb);
    assert!(heap.block(a, 3).iter().all(|unit| *unit == 0xaaaa));
    assert!(heap.block(b, 3).iter().all(|unit| *unit == 0xbbbb));
}

#[test]
fn a_freed_hole_in_the_middle_is_reused() {
    let mut memory = buffer(64);
    let mut heap = UHeap::new(&mut memory);
    let _top = heap.alloc(4).expect("fits");
    let middle = heap.alloc(6).expect("fits");
    let _bottom = heap.alloc(4).expect("fits");

    assert!(heap.free(middle), "a middle block leaves a hole");
    assert_eq!(heap.blocks(), 3, "the hole keeps its record");

    let again = heap.alloc(6).expect("the hole fits exactly");
    assert_eq!(again, middle, "and it is the same hole");
    assert_eq!(heap.blocks(), 3, "no new record was needed");
}

#[test]
fn the_snuggest_hole_is_chosen() {
    let mut memory = buffer(64);
    let mut heap = UHeap::new(&mut memory);
    let _keep = heap.alloc(2).expect("fits");
    let roomy = heap.alloc(10).expect("fits");
    let _keep2 = heap.alloc(2).expect("fits");
    let snug = heap.alloc(4).expect("fits");
    let _keep3 = heap.alloc(2).expect("fits");

    assert!(heap.free(roomy));
    assert!(heap.free(snug));

    let taken = heap.alloc(4).expect("fits");
    assert_eq!(taken, snug, "best fit, not first fit");
}

#[test]
fn adjacent_holes_merge_into_one() {
    let mut memory = buffer(64);
    let mut heap = UHeap::new(&mut memory);
    let _anchor = heap.alloc(2).expect("fits");
    let upper = heap.alloc(3).expect("fits");
    let lower = heap.alloc(3).expect("fits");
    let _floor = heap.alloc(2).expect("fits");
    assert_eq!(heap.blocks(), 4);

    assert!(heap.free(upper));
    assert!(heap.free(lower));
    assert_eq!(heap.blocks(), 3, "two holes became one record");

    // And the merged hole is usable as a whole, which neither half was.
    let big = heap.alloc(6).expect("the merged hole fits six");
    assert_eq!(big, lower, "it starts where the lower half did");
}

#[test]
fn holes_merge_in_either_order() {
    for order in [true, false] {
        let mut memory = buffer(64);
        let mut heap = UHeap::new(&mut memory);
        let _anchor = heap.alloc(2).expect("fits");
        let upper = heap.alloc(3).expect("fits");
        let lower = heap.alloc(3).expect("fits");
        let _floor = heap.alloc(2).expect("fits");

        if order {
            assert!(heap.free(upper));
            assert!(heap.free(lower));
        } else {
            assert!(heap.free(lower));
            assert!(heap.free(upper));
        }
        assert_eq!(heap.blocks(), 3, "merged whichever way round, order {order}");
        assert!(heap.alloc(6).is_some(), "and usable, order {order}");
    }
}

#[test]
fn freeing_the_end_unwinds_every_trailing_hole() {
    let mut memory = buffer(64);
    let mut heap = UHeap::new(&mut memory);
    let a = heap.alloc(3).expect("fits");
    let b = heap.alloc(3).expect("fits");
    let c = heap.alloc(3).expect("fits");

    // Free the two lowest first: they merge, but stay a hole because a live
    // block is still below... no -- `c` is the lowest, so freeing b then c
    // should unwind both.
    assert!(heap.free(b));
    assert_eq!(heap.blocks(), 3, "a hole above a live block stays a record");
    assert!(heap.free(c));
    assert_eq!(heap.blocks(), 1, "both trailing records went, leaving only a");
    assert_eq!(heap.used(), 4, "three units of a, one of its record");

    assert!(heap.free(a));
    assert_eq!(heap.blocks(), 0);
    assert_eq!(heap.used(), 0);
}

#[test]
fn the_table_and_the_blocks_cannot_collide() {
    // Four units: a record each and a block each means two allocations at
    // most, and the third must be refused rather than overwrite the table.
    let mut memory = buffer(4);
    let mut heap = UHeap::new(&mut memory);
    assert!(heap.alloc(1).is_some());
    assert!(heap.alloc(1).is_some());
    assert!(heap.alloc(1).is_none(), "the table would reach the blocks");
    assert!(heap.alloc(64).is_none(), "and an outsized ask is refused");
}

#[test]
fn nonsense_is_refused_rather_than_wrapped() {
    let mut memory = buffer(16);
    let mut heap = UHeap::new(&mut memory);
    assert!(heap.alloc(0).is_none(), "a block of nothing");
    assert!(heap.alloc(u16::MAX).is_none(), "past the size field");
    assert!(!heap.free(9), "a block that was never allocated");
    let a = heap.alloc(2).expect("fits");
    assert!(heap.free(a));
    assert!(!heap.free(a), "freeing twice says no rather than corrupting");
}

#[test]
fn churn_never_loses_the_heap() {
    // The property that matters: after any sequence of allocation and
    // release, everything handed out is distinct and inside the buffer, and
    // once it is all given back the heap is empty again.
    let mut memory = buffer(256);
    let mut heap = UHeap::new(&mut memory);
    let mut live: Vec<(u16, u16)> = Vec::new();

    let mut seed = 12345_u32;
    let mut next = move || {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        (seed >> 16) as u16
    };

    for round in 0..2000 {
        if live.len() > 3 && round % 3 == 0 {
            let at = (next() as usize) % live.len();
            let (index, _) = live.remove(at);
            assert!(heap.free(index), "round {round}: freeing a live block");
        } else {
            let units = 1 + next() % 6;
            if let Some(index) = heap.alloc(units) {
                for (other, size) in &live {
                    let overlaps = index < other + size && *other < index + units;
                    assert!(!overlaps, "round {round}: {index}+{units} overlaps {other}+{size}");
                }
                assert!(
                    (index as usize) + units as usize <= heap.units(),
                    "round {round}: past the end"
                );
                heap.block(index, units).fill(index as Unit);
                live.push((index, units));
            }
        }
        // Nothing a live block holds is ever disturbed by later traffic.
        for (index, size) in &live {
            assert!(
                heap.block(*index, *size).iter().all(|unit| *unit == *index as Unit),
                "round {round}: block {index} was written over"
            );
        }
    }

    for (index, _) in live {
        assert!(heap.free(index));
    }
    assert_eq!(heap.blocks(), 0, "everything given back leaves nothing behind");
    assert_eq!(heap.used(), 0);
}
