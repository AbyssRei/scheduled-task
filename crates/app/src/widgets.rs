//! 通用控件:枚举下拉选择(SearchableListDelegate 的静态列表实现)。

use gpui_kit::component::IndexPath;
use gpui_kit::component::searchable_list::{SearchableListDelegate, SearchableListItem};
use gpui_kit::{App, SharedString, Task, Window};

/// 静态选项:value = 选项序号。
#[derive(Clone)]
pub struct PickItem {
    pub label: String,
    pub value: usize,
}

impl SearchableListItem for PickItem {
    type Value = usize;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// 静态列表 delegate:所有枚举下拉共用。
pub struct PickDelegate {
    pub items: Vec<PickItem>,
}

impl SearchableListDelegate for PickDelegate {
    type Item = PickItem;

    fn items_count(&self, _section: usize) -> usize {
        self.items.len()
    }

    fn item(&self, ix: IndexPath) -> Option<&Self::Item> {
        if ix.section == 0 {
            self.items.get(ix.row)
        } else {
            None
        }
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SearchableListItem<Value = V>,
        V: PartialEq,
    {
        self.items
            .iter()
            .position(|i| i.value() == value)
            .map(IndexPath::new)
    }

    fn perform_search(&mut self, _: &str, _: &mut Window, _: &mut App) -> Task<()> {
        Task::ready(())
    }
}
