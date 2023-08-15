use std::collections::VecDeque;
use std::fmt::Debug;
use std::intrinsics::{prefetch_read_data, prefetch_write_data};
use std::marker::PhantomData;
use std::mem::{swap, transmute};
use std::ops::*;
use std::vec::Vec;

use core_simd::simd::Which::*;
use core_simd::simd::*;
use local::LocalCoordContext;
use std_float::StdFloat;

use crate::collections::ArrayDeque;
use crate::ffi::{CInlineVec, CVec};
use crate::graph::local::index::LocalNodeIndex;
use crate::graph::local::*;
use crate::graph::octree::LinearBitOctree;
use crate::math::*;

pub mod local;
mod octree;

pub const REGION_COORD_MASK: u8x3 = u8x3::from_array([0b11111000, 0b11111100, 0b11111000]);
pub const SECTIONS_IN_REGION: usize = 8 * 4 * 8;
pub const SECTIONS_IN_GRAPH: usize = 256 * 256 * 256;

pub const MAX_VIEW_DISTANCE: u8 = 127;
pub const MAX_WORLD_HEIGHT: u8 = 254;
pub const BFS_QUEUE_SIZE: usize =
    get_bfs_queue_max_size(MAX_VIEW_DISTANCE, MAX_WORLD_HEIGHT, true) as usize;
pub type BfsQueue = ArrayDeque<LocalNodeIndex<1>, BFS_QUEUE_SIZE>;

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct RegionSectionIndex(u8);

impl RegionSectionIndex {
    const X_MASK_SINGLE: u8 = 0b00000111;
    const Y_MASK_SINGLE: u8 = 0b00000011;
    const Z_MASK_SINGLE: u8 = 0b00000111;

    const X_MASK_SHIFT: u8 = 5;
    const Y_MASK_SHIFT: u8 = 3;
    const Z_MASK_SHIFT: u8 = 0;

    #[inline(always)]
    pub fn from_local(local_section_coord: u8x3) -> Self {
        Self(
            (local_section_coord
                & u8x3::from_array([
                    Self::X_MASK_SINGLE,
                    Self::Y_MASK_SINGLE,
                    Self::Z_MASK_SINGLE,
                ]) << u8x3::from_array([
                    Self::X_MASK_SHIFT,
                    Self::Y_MASK_SHIFT,
                    Self::Z_MASK_SHIFT,
                ]))
            .reduce_or(),
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum GraphDirection {
    NegX = 0,
    NegY = 1,
    NegZ = 2,
    PosX = 3,
    PosY = 4,
    PosZ = 5,
}

impl GraphDirection {
    pub const ORDERED: [GraphDirection; 6] = [
        GraphDirection::NegX,
        GraphDirection::NegY,
        GraphDirection::NegZ,
        GraphDirection::PosX,
        GraphDirection::PosY,
        GraphDirection::PosZ,
    ];

    #[inline(always)]
    pub const fn opposite(&self) -> GraphDirection {
        match self {
            GraphDirection::NegX => GraphDirection::PosX,
            GraphDirection::NegY => GraphDirection::PosY,
            GraphDirection::NegZ => GraphDirection::PosZ,
            GraphDirection::PosX => GraphDirection::NegX,
            GraphDirection::PosY => GraphDirection::NegY,
            GraphDirection::PosZ => GraphDirection::NegZ,
        }
    }

    /// SAFETY: if out of bounds, this will fail to assert in debug mode
    #[inline(always)]
    pub unsafe fn from_int_unchecked(val: u8) -> Self {
        debug_assert!(val <= 5);
        transmute(val)
    }
}

#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct GraphDirectionSet(u8);

impl GraphDirectionSet {
    #[inline(always)]
    pub fn from(packed: u8) -> Self {
        GraphDirectionSet(packed)
    }

    #[inline(always)]
    pub fn none() -> GraphDirectionSet {
        GraphDirectionSet(0)
    }

    #[inline(always)]
    pub fn all() -> GraphDirectionSet {
        let mut set = GraphDirectionSet::none();

        for dir in GraphDirection::ORDERED {
            set.add(dir);
        }

        set
    }

    #[inline(always)]
    pub fn single(direction: GraphDirection) -> GraphDirectionSet {
        let mut set = GraphDirectionSet::none();
        set.add(direction);
        set
    }

    #[inline(always)]
    pub fn add(&mut self, dir: GraphDirection) {
        self.0 |= 1 << dir as u8;
    }

    #[inline(always)]
    pub fn add_all(&mut self, set: GraphDirectionSet) {
        self.0 |= set.0;
    }

    #[inline(always)]
    pub fn contains(&self, dir: GraphDirection) -> bool {
        (self.0 & (1 << dir as u8)) != 0
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }
}

impl Default for GraphDirectionSet {
    fn default() -> Self {
        GraphDirectionSet::none()
    }
}

impl BitAnd for GraphDirectionSet {
    type Output = GraphDirectionSet;

    fn bitand(self, rhs: Self) -> Self::Output {
        GraphDirectionSet(self.0 & rhs.0)
    }
}

impl IntoIterator for GraphDirectionSet {
    type Item = GraphDirection;
    type IntoIter = GraphDirectionSetIter;

    fn into_iter(self) -> Self::IntoIter {
        GraphDirectionSetIter(self.0)
    }
}

#[repr(transparent)]
pub struct GraphDirectionSetIter(u8);

impl Iterator for GraphDirectionSetIter {
    type Item = GraphDirection;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        // Description of the iteration approach on daniel lemire's blog
        // https://lemire.me/blog/2018/02/21/iterating-over-set-bits-quickly/
        if self.0 != 0 {
            // SAFETY: the result from a valid GraphDirectionSet value should never be out of bounds
            let direction =
                unsafe { GraphDirection::from_int_unchecked(self.0.trailing_zeros() as u8) };
            self.0 &= (self.0 - 1);
            Some(direction)
        } else {
            None
        }
    }
}

// todo: should the top bit signify if it's populated or not?
#[derive(Default, Clone, Copy)]
#[repr(transparent)]
pub struct VisibilityData(u16);

impl VisibilityData {
    #[inline(always)]
    pub fn pack(mut raw: u64) -> Self {
        raw >>= 6;
        let mut packed = (raw & 0b1) as u16;
        raw >>= 5;
        packed |= (raw & 0b110) as u16;
        raw >>= 4;
        packed |= (raw & 0b111000) as u16;
        raw >>= 3;
        packed |= (raw & 0b1111000000) as u16;
        raw >>= 2;
        packed |= (raw & 0b111110000000000) as u16;

        VisibilityData(packed)
    }

    #[inline(always)]
    pub fn get_outgoing_directions(&self, incoming: GraphDirectionSet) -> GraphDirectionSet {
        // extend everything to u32s because we can shift them faster on x86 without avx512
        let vis_bits = Simd::<u32, 5>::splat(self.0 as u32);
        let in_bits = Simd::<u32, 5>::splat(incoming.0 as u32);

        let rows_cols = (vis_bits >> Simd::from_array([0_u32, 1_u32, 3_u32, 6_u32, 10_u32])).cast()
            & Simd::from_array([0b1_u32, 0b11_u32, 0b111_u32, 0b1111_u32, 0b11111_u32]);

        let rows = (rows_cols & in_bits)
            .cast::<i32>()
            .simd_ne(Simd::splat(0))
            .select(
                Simd::from_array([0b10_u32, 0b100_u32, 0b1000_u32, 0b10000_u32, 0b100000_u32]),
                Simd::splat(0_u32),
            );

        let cols = ((in_bits
            << Simd::from_array([
                u32::BITS - 2,
                u32::BITS - 3,
                u32::BITS - 4,
                u32::BITS - 5,
                u32::BITS - 6,
            ]))
        .cast::<i32>()
            >> Simd::splat((u32::BITS - 1) as i32))
        .cast::<u32>()
            & rows_cols;

        // extend to po2 vectors to make the reduction happy
        let outgoing_bits = simd_swizzle!(
            rows,
            cols,
            [
                First(0),
                First(1),
                First(2),
                First(3),
                First(4),
                First(4),
                First(4),
                First(4),
                Second(0),
                Second(1),
                Second(2),
                Second(3),
                Second(4),
                Second(4),
                Second(4),
                Second(4),
            ]
        )
        .reduce_or() as u8; // & !incoming

        GraphDirectionSet(outgoing_bits)
    }
}

pub const fn get_bfs_queue_max_size(
    section_render_distance: u8,
    world_height: u8,
    frustum: bool,
) -> u32 {
    // for the worst case, we will assume the player is in the center of the render distance and
    // world height.
    // for traversal lengths, we don't include the chunk the player is in.

    let max_height_traversal = (world_height.div_ceil(2) - 1) as u32;
    let max_width_traversal = section_render_distance as u32;

    // the 2 accounts for the chunks directly above and below the player
    let mut count = 2;
    let mut layer_index = 1_u32;

    // check if the traversal up and down is restricted by the world height. if so, remove the
    // out-of-bounds layers from the iteration
    if max_height_traversal < max_width_traversal {
        count = 0;
        layer_index = max_width_traversal - max_height_traversal;
    }

    // add rings that are on both the top and bottom.
    // simplification of:
    // while layer_index < max_width_traversal {
    //     count += (layer_index * 8);
    //     layer_index += 1;
    // }
    count += 4 * (max_width_traversal - layer_index) * (max_width_traversal + layer_index - 1);

    // add final, outer-most ring.
    count += (max_width_traversal * 4);

    if frustum {
        // divide by 2 because the player should never be able to see more than half of the world
        // at once with frustum culling. This assumes an FOV maximum of 180 degrees.
        count = count.div_ceil(2);
    }

    count
}

pub struct BfsCachedState {
    incoming_directions: [GraphDirectionSet; SECTIONS_IN_GRAPH],
}

impl BfsCachedState {
    pub fn reset(&mut self) {
        self.incoming_directions.fill(GraphDirectionSet::none());
    }
}

impl Default for BfsCachedState {
    fn default() -> Self {
        BfsCachedState {
            incoming_directions: [GraphDirectionSet::default(); SECTIONS_IN_GRAPH],
        }
    }
}

pub struct Graph {
    section_has_geometry_bits: LinearBitOctree,
    section_is_visible_bits: LinearBitOctree,

    section_visibility_bit_sets: [VisibilityData; SECTIONS_IN_GRAPH],

    bfs_cached_state: BfsCachedState,
}

impl Graph {
    pub fn new() -> Self {
        Graph {
            section_has_geometry_bits: Default::default(),
            section_is_visible_bits: Default::default(),
            section_visibility_bit_sets: [Default::default(); SECTIONS_IN_GRAPH],
            bfs_cached_state: Default::default(),
        }
    }

    pub fn cull(&mut self, coord_context: &LocalCoordContext, no_occlusion_cull: bool) {
        self.section_is_visible_bits.clear();

        self.frustum_and_fog_cull(coord_context);
        self.bfs_and_occlusion_cull(coord_context, no_occlusion_cull);
    }

    fn frustum_and_fog_cull(&mut self, coord_context: &LocalCoordContext) {
        let mut level_3_index = coord_context.iter_node_origin_index;

        // this could go more linearly in memory, but we probably have good enough locality inside
        // the level 3 nodes
        for _x in 0..coord_context.level_3_node_iters.x() {
            for _y in 0..coord_context.level_3_node_iters.y() {
                for _z in 0..coord_context.level_3_node_iters.z() {
                    self.check_node(level_3_index, coord_context);

                    level_3_index = level_3_index.inc_z();
                }
                level_3_index = level_3_index.inc_y();
            }
            level_3_index = level_3_index.inc_x();
        }
    }

    #[inline(always)]
    fn check_node<const LEVEL: u8>(
        &mut self,
        index: LocalNodeIndex<LEVEL>,
        coord_context: &LocalCoordContext,
    ) {
        match coord_context.test_node(index) {
            BoundsCheckResult::Outside => {}
            BoundsCheckResult::Inside => {
                self.section_is_visible_bits
                    .copy_from(&self.section_has_geometry_bits, index);
            }
            BoundsCheckResult::Partial => match LEVEL {
                3 => {
                    for lower_node_index in index.iter_lower_nodes::<2>() {
                        self.check_node(lower_node_index, coord_context);
                    }
                }
                2 => {
                    for lower_node_index in index.iter_lower_nodes::<1>() {
                        self.check_node(lower_node_index, coord_context);
                    }
                }
                1 => {
                    for lower_node_index in index.iter_lower_nodes::<0>() {
                        self.check_node(lower_node_index, coord_context);
                    }
                }
                0 => {
                    self.section_is_visible_bits
                        .copy_from(&self.section_has_geometry_bits, index);
                }
                _ => panic!("Invalid node level: {}", LEVEL),
            },
        }
    }

    fn bfs_and_occlusion_cull(
        &mut self,
        coord_context: &LocalCoordContext,
        no_occlusion_cull: bool,
    ) {
        let mut read_queue = BfsQueue::default();
        let mut write_queue = BfsQueue::default();

        read_queue.push(coord_context.camera_section_idx);

        let mut read_queue_ref = &mut read_queue;
        let mut write_queue_ref = &mut write_queue;

        let mut finished = false;
        while !finished {
            finished = true;

            while let Some(node_index) = read_queue.pop() {
                finished = false;

                let node_pos = node_index.unpack();
                let outgoing_directions = coord_context.get_valid_directions(node_pos);

                let neighbor_indices = node_index.get_all_neighbors();

                for direction in outgoing_directions {
                    let neighbor = neighbor_indices.get(direction);

                    // the outgoing direction for the current node is the incoming direction for the neighbor
                    let incoming_direction = direction.opposite();

                    // SAFETY: LocalNodeIndex should never have the top 8 bits set, and the array is exactly
                    // 2^24 elements long.
                    let node_incoming_directions = unsafe {
                        self.bfs_cached_state
                            .incoming_directions
                            .get_unchecked_mut(node_index.as_array_offset())
                    };

                    let should_enqueue = node_incoming_directions.is_empty();

                    node_incoming_directions.add(incoming_direction);

                    unsafe {
                        write_queue.push_conditionally_unchecked(node_index, should_enqueue);
                    }
                }
            }

            read_queue.reset();
            write_queue.reset();
            swap(&mut read_queue, &mut write_queue);

            self.bfs_cached_state.reset();
        }
    }

    fn divide_graph_into_regions(&self) -> CVec<RegionDrawBatch> {
        // let mut sorted_batches: Vec<RegionDrawBatch> = Vec::new();
        // CVec::from_boxed_slice(sorted_batches.into_boxed_slice())
        todo!()
    }

    pub fn add_section(
        &mut self,
        section_coord: u8x3,
        has_geometry: bool,
        visibility_data: VisibilityData,
    ) {
        let index = LocalNodeIndex::<3>::pack(section_coord);

        self.section_has_geometry_bits.set(index, has_geometry);
        self.section_visibility_bit_sets[index.as_array_offset()] = visibility_data;
    }

    pub fn remove_section(&mut self, section_coord: u8x3) {
        let index = LocalNodeIndex::<1>::pack(section_coord);

        self.section_has_geometry_bits.set(index, false);
        self.section_visibility_bit_sets[index.as_array_offset()] = Default::default();
    }
}

#[repr(C)]
pub struct RegionDrawBatch {
    region_coord: (i32, i32, i32),
    sections: CInlineVec<RegionSectionIndex, SECTIONS_IN_REGION>,
}

impl RegionDrawBatch {
    pub fn new(region_coord: i32x3) -> Self {
        RegionDrawBatch {
            region_coord: region_coord.into_tuple(),
            sections: CInlineVec::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}