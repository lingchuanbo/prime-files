use super::{TAGGED, Tags, item_key};
use gpui::App;
use std::collections::HashMap;

impl Tags {
    pub(crate) fn moved_batch(changes: &HashMap<String, String>, cx: &mut App) {
        let mapping: HashMap<_, _> = changes
            .iter()
            .map(|(source, target)| (item_key(source), target.clone()))
            .collect();
        Self::change(cx, |tags| {
            tags.remap_batch(&mapping);
            if let Ok(mut slot) = TAGGED.lock()
                && let Some(workspaces) = slot.as_mut()
            {
                for paths in workspaces.values_mut() {
                    for path in paths {
                        if let Some(target) = mapping.get(&item_key(path)) {
                            *path = target.clone();
                        }
                    }
                }
            }
        });
    }

    fn remap_batch(&mut self, mapping: &HashMap<String, String>) {
        // 从快照一次性映射，避免 A/B 互换时覆盖另一个项目的标签或颜色。
        self.items = std::mem::take(&mut self.items)
            .into_iter()
            .map(|(key, (path, ids))| match mapping.get(&key) {
                Some(target) => (item_key(target), (target.clone(), ids)),
                None => (key, (path, ids)),
            })
            .collect();
        self.card_colors = std::mem::take(&mut self.card_colors)
            .into_iter()
            .map(|((workspace, path), color)| {
                let target = mapping
                    .get(&path)
                    .map(|path| item_key(path))
                    .unwrap_or(path);
                ((workspace, target), color)
            })
            .collect();
        for order in self.orders.values_mut() {
            for path in order {
                if let Some(target) = mapping.get(path) {
                    *path = item_key(target);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_rename_swaps_workspace_membership_colors_and_order_without_loss() {
        let mut tags = Tags::default();
        let (a, b) = ("D:/a.txt", "D:/b.txt");
        tags.items.insert(item_key(a), (a.into(), vec![1]));
        tags.items.insert(item_key(b), (b.into(), vec![2]));
        tags.orders.insert(1, vec![item_key(a), item_key(b)]);
        tags.set_card_color_for(1, a, Some(0xff0000));
        tags.set_card_color_for(1, b, Some(0x0000ff));
        let mapping = HashMap::from([(item_key(a), b.into()), (item_key(b), a.into())]);
        tags.remap_batch(&mapping);
        assert_eq!(vec![2], tags.items[&item_key(a)].1);
        assert_eq!(vec![1], tags.items[&item_key(b)].1);
        assert_eq!(Some(0x0000ff), tags.card_color_for(1, a));
        assert_eq!(Some(0xff0000), tags.card_color_for(1, b));
        assert_eq!(vec![item_key(b), item_key(a)], tags.orders[&1]);
        tags.remap_batch(&mapping);
        assert_eq!(vec![1], tags.items[&item_key(a)].1);
        assert_eq!(Some(0xff0000), tags.card_color_for(1, a));
    }
}
