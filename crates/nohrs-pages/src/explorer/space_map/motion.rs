use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use nohrs_services::fs::space::{LayoutOptions, Rect, SpaceNode, Tile};

const TILE_GAP: f32 = 5.0;
const REFLOW_DURATION: Duration = Duration::from_millis(480);

pub(super) fn layout_options() -> LayoutOptions {
    LayoutOptions {
        padding: TILE_GAP / 2.0,
        padding_outer: TILE_GAP / 2.0,
        ..LayoutOptions::default()
    }
}

// 保留子节点索引，避免删除后选中项和动画对应到其他文件。
pub(super) fn remove_path(node: &mut SpaceNode, path: &str) -> u64 {
    if crate::explorer::same_path(&node.path, path) {
        let removed = node.bytes;
        clear_sizes(node);
        return removed;
    }
    let removed = node
        .children
        .iter_mut()
        .map(|child| remove_path(child, path))
        .sum();
    node.bytes = node.bytes.saturating_sub(removed);
    removed
}

fn clear_sizes(node: &mut SpaceNode) {
    node.bytes = 0;
    for child in &mut node.children {
        clear_sizes(child);
    }
}

pub(super) struct Transition {
    pairs: Vec<(Tile, Tile)>,
    target: Rc<Vec<Tile>>,
    started: Instant,
}

impl Transition {
    pub(super) fn new(before: Rc<Vec<Tile>>, target: Rc<Vec<Tile>>, started: Instant) -> Self {
        let targets: HashMap<&[usize], &Tile> = target
            .iter()
            .map(|tile| (tile.crumbs.as_slice(), tile))
            .collect();
        let mut pairs: Vec<(Tile, Tile)> = before
            .iter()
            .map(|tile| {
                let end = targets
                    .get(tile.crumbs.as_slice())
                    .copied()
                    .cloned()
                    .unwrap_or_else(|| collapsed(tile));
                (tile.clone(), end)
            })
            .collect();
        let previous: std::collections::HashSet<&[usize]> =
            before.iter().map(|tile| tile.crumbs.as_slice()).collect();
        pairs.extend(
            target
                .iter()
                .filter(|tile| !previous.contains(tile.crumbs.as_slice()))
                .map(|tile| (collapsed(tile), tile.clone())),
        );
        Self {
            pairs,
            target,
            started,
        }
    }

    pub(super) fn finished(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= REFLOW_DURATION
    }

    pub(super) fn frame(&self, now: Instant) -> Rc<Vec<Tile>> {
        if self.finished(now) {
            return Rc::clone(&self.target);
        }
        let elapsed = now.saturating_duration_since(self.started).as_secs_f32();
        let progress = (elapsed / REFLOW_DURATION.as_secs_f32()).clamp(0.0, 1.0);
        let amount = progress * progress * (3.0 - 2.0 * progress);
        Rc::new(
            self.pairs
                .iter()
                .map(|(start, end)| Tile {
                    rect: interpolate(start.rect, end.rect, amount),
                    header: start
                        .header
                        .zip(end.header)
                        .map(|(start, end)| interpolate(start, end, amount)),
                    crumbs: end.crumbs.clone(),
                    depth: end.depth,
                })
                .collect(),
        )
    }
}

fn collapsed(tile: &Tile) -> Tile {
    let mut collapsed = tile.clone();
    collapsed.rect = Rect::new(
        tile.rect.x + tile.rect.w / 2.0,
        tile.rect.y + tile.rect.h / 2.0,
        0.0,
        0.0,
    );
    collapsed.header = tile.header.map(|header| {
        Rect::new(
            header.x + header.w / 2.0,
            header.y + header.h / 2.0,
            0.0,
            0.0,
        )
    });
    collapsed
}

fn interpolate(start: Rect, end: Rect, amount: f32) -> Rect {
    let blend = |start: f32, end: f32| start + (end - start) * amount;
    Rect::new(
        blend(start.x, end.x),
        blend(start.y, end.y),
        blend(start.w, end.w),
        blend(start.h, end.h),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nohrs_services::fs::space::layout;

    fn file(path: &str, bytes: u64) -> SpaceNode {
        SpaceNode {
            name: path.to_string(),
            path: path.to_string(),
            bytes,
            files: 1,
            hidden_files: 0,
            modified: 0,
            hidden: false,
            hidden_bytes: 0,
            is_dir: false,
            count: 1,
            children: Vec::new(),
        }
    }

    fn tree() -> SpaceNode {
        SpaceNode {
            name: "root".into(),
            path: "root".into(),
            bytes: 100,
            files: 2,
            hidden_files: 0,
            modified: 0,
            hidden: false,
            hidden_bytes: 0,
            is_dir: true,
            count: 1,
            children: vec![file("big", 60), file("small", 40)],
        }
    }

    #[test]
    fn siblings_have_exactly_five_pixels_between_them() {
        let tiles = layout(
            &tree(),
            Rect::new(0.0, 0.0, 400.0, 200.0),
            &layout_options(),
        );
        let first = &tiles[0].rect;
        let second = &tiles[1].rect;
        assert_eq!(5.0, second.x - first.right());
    }

    #[test]
    fn deleting_keeps_node_indices_and_updates_ancestor_sizes() {
        let mut root = tree();
        assert_eq!(60, remove_path(&mut root, "big"));
        assert_eq!(40, root.bytes);
        assert_eq!(0, root.children[0].bytes);
        assert_eq!("small", root.children[1].path);
        assert_eq!(0, remove_path(&mut root, "missing"));
        assert_eq!(40, root.bytes);
    }

    #[test]
    fn deleting_a_directory_cannot_subtract_its_children_twice() {
        let mut root = tree();
        let mut directory = tree();
        directory.path = "nested".into();
        root.bytes = directory.bytes;
        root.children = vec![directory];

        assert_eq!(100, remove_path(&mut root, "nested"));
        assert_eq!(0, remove_path(&mut root, "big"));
        assert_eq!(0, root.bytes);
        assert_eq!(0, root.children[0].children[0].bytes);
    }

    #[test]
    fn deleting_the_last_tile_animates_to_an_empty_map() {
        let root = tree();
        let before = Rc::new(layout(
            &root,
            Rect::new(0.0, 0.0, 400.0, 200.0),
            &layout_options(),
        ));
        let now = Instant::now();
        let transition = Transition::new(before.clone(), Rc::new(Vec::new()), now);

        assert_eq!(before.as_ref(), transition.frame(now).as_ref());
        assert!(transition.frame(now + REFLOW_DURATION / 2)[0].rect.w > 0.0);
        assert!(transition.frame(now + REFLOW_DURATION).is_empty());
        assert!(transition.finished(now + REFLOW_DURATION));
    }

    #[test]
    fn deletion_shrinks_the_removed_tile_and_expands_the_survivor() {
        let mut root = tree();
        let area = Rect::new(0.0, 0.0, 400.0, 200.0);
        let before = Rc::new(layout(&root, area, &layout_options()));
        remove_path(&mut root, "big");
        let after = Rc::new(layout(&root, area, &layout_options()));
        let now = Instant::now();
        let transition = Transition::new(before.clone(), after.clone(), now);
        let start = transition.frame(now);
        let middle = transition.frame(now + REFLOW_DURATION / 2);
        let end = transition.frame(now + REFLOW_DURATION);

        assert_eq!(before.as_ref(), start.as_ref());
        assert!(middle[0].rect.w < start[0].rect.w);
        assert!(middle[1].rect.w > start[1].rect.w);
        assert_eq!(after.as_ref(), end.as_ref());
        assert_eq!(vec![1], end[0].crumbs);
    }
}
