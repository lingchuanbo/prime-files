use nohrs_core::config;
use nohrs_core::t;
use nohrs_ui::components::file_list::FileListDelegate;

use gpui::{AppContext, Context, Window};
use gpui_component::list::{ListEvent, ListState};

use super::ExplorerPane;
use super::types::ResizingColumn;

impl ExplorerPane {
    pub(crate) fn ensure_list_initialized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.list.is_none() {
            let mut delegate = FileListDelegate::new();
            delegate.set_items(self.filtered_entries.clone());
            let list = cx.new(|cx| ListState::new(delegate, window, cx));
            let sub = cx.subscribe_in(
                &list,
                window,
                |this, _list, event: &ListEvent, window, cx| match event {
                    ListEvent::Select(ix) => {
                        this.select_single(ix.row);
                        if let Some(item) = this.filtered_entries.get(ix.row).cloned() {
                            this.open_preview(item.path, window, cx);
                        }
                    }
                    ListEvent::Confirm(ix) => {
                        if let Some(info) = this.last_click_info.as_ref()
                            && info.row == ix.row
                            && info.timestamp.elapsed() < config::CONFIRM_SUPPRESS_WINDOW
                            && info.click_count >= 2
                        {
                            this.last_click_info = None;
                            return;
                        }
                        this.last_click_info = None;
                        this.select_single(ix.row);
                        if let Some(item) = this.filtered_entries.get(ix.row).cloned() {
                            this.activate_entry(item, window, cx);
                        }
                    }
                    ListEvent::Cancel => {}
                },
            );
            self.subs.push(sub);
            self.list = Some(list);
        } else if let Some(list) = &self.list {
            let items = self.filtered_entries.clone();
            list.update(cx, |l, _cx| {
                l.delegate_mut().set_items(items);
            });
        }
    }

    pub(crate) fn start_column_resize(
        &mut self,
        column_index: usize,
        start_pos: gpui::Point<gpui::Pixels>,
    ) {
        let start_width = match column_index {
            0 => self.col_name_width,
            1 => self.col_type_width,
            2 => self.col_size_width,
            3 => self.col_modified_width,
            _ => return,
        };

        self.resizing_column = Some(ResizingColumn {
            column_index,
            start_width,
            start_x: start_pos,
        });
    }

    pub(crate) fn update_column_resize(&mut self, current_pos: gpui::Point<gpui::Pixels>) {
        if let Some(resize) = self.resizing_column {
            let delta: f32 = (current_pos.x - resize.start_x.x).into();
            let new_width = (resize.start_width + delta).max(config::MIN_COLUMN_WIDTH);

            match resize.column_index {
                0 => self.col_name_width = new_width,
                1 => self.col_type_width = new_width,
                2 => self.col_size_width = new_width,
                3 => self.col_modified_width = new_width,
                _ => {}
            }

            self.update_item_sizes();
        }
    }

    pub(crate) fn stop_column_resize(&mut self) {
        self.resizing_column = None;
    }

    fn column_width_mut(&mut self, column: usize) -> Option<&mut f32> {
        match column {
            0 => Some(&mut self.col_name_width),
            1 => Some(&mut self.col_type_width),
            2 => Some(&mut self.col_size_width),
            3 => Some(&mut self.col_modified_width),
            _ => None,
        }
    }

    /// Whether `column` is shown (Name always is).
    pub(crate) fn column_visible(&self, column: usize) -> bool {
        column == 0
            || self
                .hidden_column_widths
                .get(column)
                .is_some_and(|width| *width == 0.0)
    }

    /// Hides or shows `column`, keeping the width it had. Name stays.
    pub(crate) fn toggle_column(&mut self, column: usize) {
        if column == 0 || column >= self.hidden_column_widths.len() {
            return;
        }
        let saved = self.hidden_column_widths[column];
        let Some(width) = self.column_width_mut(column) else {
            return;
        };
        if saved == 0.0 {
            let current = std::mem::replace(width, 0.0);
            self.hidden_column_widths[column] = current.max(config::MIN_COLUMN_WIDTH);
        } else {
            *width = saved;
            self.hidden_column_widths[column] = 0.0;
        }
        self.update_item_sizes();
    }

    /// Sizes `column` to its widest value among the entries on screen, as
    /// Explorer's "Size column to fit". Text is measured by an average glyph
    /// width: close enough for a column, and needs no text layout.
    pub(crate) fn fit_column(&mut self, column: usize) {
        use nohrs_ui::components::file_list::{format_date, get_file_type, human_bytes};
        const GLYPH: f32 = 7.2;
        const PADDING: f32 = 20.0;
        if !self.column_visible(column) {
            return;
        }
        let longest = |text: String| text.chars().count();
        let widest = self
            .filtered_entries
            .iter()
            .map(|entry| match column {
                0 => longest(entry.name.clone()),
                1 => longest(get_file_type(&entry.name, &entry.kind)),
                2 => longest(human_bytes(entry.size)),
                _ => longest(format_date(&entry.modified)),
            })
            .max()
            .unwrap_or(0);
        let heading = [t!("Nome"), t!("Tipo"), t!("Tamanho"), t!("Modificado")]
            .get(column)
            .map_or(0, |label| label.chars().count() + 2);
        let indent = if column == 0 { view_name_indent() } else { 0.0 };
        let fitted = (indent + widest.max(heading) as f32 * GLYPH + PADDING)
            .clamp(config::MIN_COLUMN_WIDTH, 900.0);
        if let Some(width) = self.column_width_mut(column) {
            *width = fitted;
        }
        self.update_item_sizes();
    }
}

// The name column's chevron gutter and icon, which its text starts after.
fn view_name_indent() -> f32 {
    super::view::listing::row::NAME_INDENT
}
