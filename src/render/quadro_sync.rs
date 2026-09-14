// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//! QuadroSync board discovery.
//!
//! Each nvidia-drm device reports at most one QuadroSync board (the one wired
//! to its own display engine) together with the GPU ids bound to that board.
//! Two devices share a board exactly when each one's board lists the other's
//! GPU id. Grouping devices by that relation, transitively, yields the boards
//! present in the machine without any user configuration.

/// One device's QuadroSync facts, as reported by the driver.
#[derive(Debug, Clone)]
pub struct BoardMember {
    /// Index into `DcsState::devices`.
    pub device_index: usize,
    /// This device's own GPU id.
    pub gpu_id: u32,
    /// GPU ids bound to the board attached to this device.
    pub gpu_ids: Vec<u32>,
}

/// A physical QuadroSync board and the DCS devices attached to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuadroSyncBoard {
    /// Small stable id: boards are numbered 0.. in order of their lowest
    /// device index.
    pub id: u32,
    /// Devices attached to this board, ascending.
    pub device_indices: Vec<usize>,
}

/// Group devices into boards. Two members share a board when each one's
/// `gpu_ids` contains the other's `gpu_id`; the relation is closed
/// transitively so a board carrying three or four GPUs forms one group.
pub fn group_boards(members: &[BoardMember]) -> Vec<QuadroSyncBoard> {
    let n = members.len();
    // Union-find over member positions.
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }
    for i in 0..n {
        for j in (i + 1)..n {
            let shared = members[i].gpu_ids.contains(&members[j].gpu_id)
                && members[j].gpu_ids.contains(&members[i].gpu_id);
            if shared {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[rj] = ri;
                }
            }
        }
    }

    // Collect groups, each sorted by device index.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut root_to_group: Vec<Option<usize>> = vec![None; n];
    for (i, member) in members.iter().enumerate() {
        let root = find(&mut parent, i);
        let slot = match root_to_group[root] {
            Some(slot) => slot,
            None => {
                groups.push(Vec::new());
                root_to_group[root] = Some(groups.len() - 1);
                groups.len() - 1
            }
        };
        groups[slot].push(member.device_index);
    }
    for group in &mut groups {
        group.sort_unstable();
    }
    // Stable numbering: by lowest device index.
    groups.sort_by_key(|g| g[0]);

    groups
        .into_iter()
        .enumerate()
        .map(|(id, device_indices)| QuadroSyncBoard {
            id: id as u32,
            device_indices,
        })
        .collect()
}

/// The id of the board containing `device_index`, if any.
pub fn board_for_device(boards: &[QuadroSyncBoard], device_index: usize) -> Option<u32> {
    boards
        .iter()
        .find(|b| b.device_indices.contains(&device_index))
        .map(|b| b.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(device_index: usize, gpu_id: u32, gpu_ids: &[u32]) -> BoardMember {
        BoardMember { device_index, gpu_id, gpu_ids: gpu_ids.to_vec() }
    }

    #[test]
    fn one_board_bridging_two_gpus() {
        let boards = group_boards(&[
            member(0, 0x100, &[0x100, 0x200]),
            member(1, 0x200, &[0x100, 0x200]),
        ]);
        assert_eq!(boards, vec![QuadroSyncBoard { id: 0, device_indices: vec![0, 1] }]);
    }

    #[test]
    fn two_chained_boards_one_per_gpu() {
        let boards = group_boards(&[
            member(0, 0x100, &[0x100]),
            member(1, 0x200, &[0x200]),
        ]);
        assert_eq!(
            boards,
            vec![
                QuadroSyncBoard { id: 0, device_indices: vec![0] },
                QuadroSyncBoard { id: 1, device_indices: vec![1] },
            ]
        );
    }

    #[test]
    fn three_gpus_on_one_board_group_transitively() {
        // Device 2's board lists all three; devices 0 and 1 each list only
        // themselves plus device 2. Mutual with 2 chains them together.
        let boards = group_boards(&[
            member(0, 0x100, &[0x100, 0x300]),
            member(1, 0x200, &[0x200, 0x300]),
            member(2, 0x300, &[0x100, 0x200, 0x300]),
        ]);
        assert_eq!(boards, vec![QuadroSyncBoard { id: 0, device_indices: vec![0, 1, 2] }]);
    }

    #[test]
    fn one_sided_listing_is_not_shared() {
        // Device 0's board claims device 1, but device 1's board does not
        // claim device 0: not mutual, so separate boards.
        let boards = group_boards(&[
            member(0, 0x100, &[0x100, 0x200]),
            member(1, 0x200, &[0x200]),
        ]);
        assert_eq!(boards.len(), 2);
    }

    #[test]
    fn ids_follow_lowest_device_index_regardless_of_input_order() {
        let boards = group_boards(&[
            member(1, 0x200, &[0x200]),
            member(0, 0x100, &[0x100]),
        ]);
        assert_eq!(boards[0], QuadroSyncBoard { id: 0, device_indices: vec![0] });
        assert_eq!(boards[1], QuadroSyncBoard { id: 1, device_indices: vec![1] });
    }

    #[test]
    fn no_members_no_boards() {
        assert!(group_boards(&[]).is_empty());
    }
}
