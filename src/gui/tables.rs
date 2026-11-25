use super::components::{BtnVariant, Theme};
use super::{BridgeApp, numeric_font};
use gpui::{AnyElement, Context, Div, FontWeight, div, prelude::*, px};
use std::{cmp::Ordering, ops::Range};

const TABLE_ROWS_PER_PAGE: usize = 10;

#[derive(Clone, Copy)]
pub(super) struct TablePage {
    pub(super) index: usize,
    count: usize,
    total: usize,
}

impl TablePage {
    pub(super) fn new(total: usize, requested: usize) -> Self {
        let count = total.div_ceil(TABLE_ROWS_PER_PAGE);
        Self {
            index: requested.min(count.saturating_sub(1)),
            count,
            total,
        }
    }

    pub(super) fn range(self) -> Range<usize> {
        let start = self.index * TABLE_ROWS_PER_PAGE;
        start..start.saturating_add(TABLE_ROWS_PER_PAGE).min(self.total)
    }

    pub(super) fn has_multiple_pages(self) -> bool {
        self.count > 1
    }

    fn previous(self) -> Option<usize> {
        self.index.checked_sub(1)
    }

    fn next(self) -> Option<usize> {
        (self.index + 1 < self.count).then_some(self.index + 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NumberSort {
    Default,
    Descending,
    Ascending,
}

impl NumberSort {
    pub(super) fn cycle(self) -> Self {
        match self {
            Self::Default => Self::Descending,
            Self::Descending => Self::Ascending,
            Self::Ascending => Self::Default,
        }
    }

    pub(super) fn compare(self, left: u64, right: u64) -> Ordering {
        match self {
            Self::Default => Ordering::Equal,
            Self::Descending => right.cmp(&left),
            Self::Ascending => left.cmp(&right),
        }
    }

    pub(super) fn arrow(self) -> &'static str {
        match self {
            Self::Default => "↕",
            Self::Descending => "↓",
            Self::Ascending => "↑",
        }
    }
}

impl BridgeApp {
    pub(super) fn table_frame(t: Theme) -> Div {
        Self::card(t).p(px(0.0)).overflow_hidden().flex().flex_col()
    }

    pub(super) fn table_row(t: Theme) -> Div {
        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(14.0))
            .min_h(px(48.0))
            .border_t_1()
            .border_color(t.border_subtle)
    }

    pub(super) fn table_header(t: Theme) -> Div {
        Self::table_row(t)
            .min_h(px(35.0))
            .bg(t.bg_well)
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.text_muted)
    }

    pub(super) fn table_cell(width: f32) -> Div {
        div().w(px(width)).flex_shrink_0().truncate()
    }

    pub(super) fn sortable_column(
        t: Theme,
        width: f32,
        id: &'static str,
        label: &'static str,
        order: NumberSort,
        on_sort: fn(&mut Self),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Self::table_cell(width)
            .id(id)
            .flex()
            .items_center()
            .justify_end()
            .gap(px(4.0))
            .h(px(28.0))
            .cursor_pointer()
            .focusable()
            .tab_stop(true)
            .when(t.keyboard_focus, |el| {
                el.focus(move |style| style.text_color(t.blue_text))
            })
            .hover(move |style| style.text_color(t.text_primary))
            .when(order != NumberSort::Default, |el| {
                el.text_color(t.text_primary)
            })
            .child(label)
            .child(order.arrow())
            .on_click(cx.listener(move |this, _, _, cx| {
                on_sort(this);
                cx.notify();
            }))
            .into_any_element()
    }

    pub(super) fn table_footer(
        t: Theme,
        id: &'static str,
        page: TablePage,
        on_page: fn(&mut Self, usize),
        cx: &mut Context<Self>,
    ) -> Div {
        let range = page.range();
        let page_button = |suffix, label, target: Option<usize>, cx: &mut Context<Self>| {
            Self::button(
                t,
                format!("{id}-{suffix}"),
                None,
                false,
                label,
                if target.is_some() {
                    BtnVariant::Secondary
                } else {
                    BtnVariant::Disabled
                },
                move |this, _, _| {
                    if let Some(target) = target {
                        on_page(this, target);
                    }
                },
                cx,
            )
        };
        Self::table_row(t)
            .justify_between()
            .text_color(t.text_muted)
            .text_size(px(11.5))
            .child(div().font(numeric_font()).child(format!(
                "第 {}–{} 条，共 {} 条",
                range.start + 1,
                range.end,
                page.total
            )))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(page_button("previous", "上一页", page.previous(), cx))
                    .child(div().font(numeric_font()).child(format!(
                        "{} / {}",
                        page.index + 1,
                        page.count
                    )))
                    .child(page_button("next", "下一页", page.next(), cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_ranges_cover_records_and_clamp_after_filtering() {
        let first = TablePage::new(23, 0);
        assert_eq!(first.range(), 0..10);
        assert!(first.has_multiple_pages());
        assert_eq!(first.previous(), None);
        assert_eq!(first.next(), Some(1));

        let last = TablePage::new(23, 2);
        assert_eq!(last.range(), 20..23);
        assert_eq!(last.previous(), Some(1));
        assert_eq!(last.next(), None);

        let filtered = TablePage::new(3, 2);
        assert_eq!(filtered.index, 0);
        assert_eq!(filtered.range(), 0..3);
        assert!(!filtered.has_multiple_pages());
        assert_eq!(TablePage::new(0, 2).range(), 0..0);
        assert_eq!(TablePage::new(300, 29).range(), 290..300);
    }

    #[test]
    fn numeric_sort_cycles_through_both_directions() {
        let descending = NumberSort::Default.cycle();
        let ascending = descending.cycle();
        assert_eq!(descending.compare(10, 2), Ordering::Less);
        assert_eq!(ascending.compare(10, 2), Ordering::Greater);
        assert_eq!(ascending.cycle(), NumberSort::Default);
    }
}
