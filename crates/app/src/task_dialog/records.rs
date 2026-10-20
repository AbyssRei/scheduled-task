//! 带稳定身份的表单记录；异步插入和拖动均不依赖旧下标。

pub(super) struct Record<T> {
    pub id: u64,
    pub value: T,
}

pub(super) struct Records<T> {
    pub rows: Vec<Record<T>>,
    next_id: u64,
}

impl<T> Default for Records<T> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            next_id: 0,
        }
    }
}

impl<T> Records<T> {
    /// 无锚点或锚点已删除时追加；批量结果保持原始顺序。
    pub fn insert_after(&mut self, after: Option<u64>, values: impl IntoIterator<Item = T>) {
        let index = after
            .and_then(|id| self.rows.iter().position(|row| row.id == id))
            .map_or(self.rows.len(), |index| index + 1);
        let rows = values
            .into_iter()
            .map(|value| {
                let id = self.next_id;
                self.next_id += 1;
                Record { id, value }
            })
            .collect::<Vec<_>>();
        self.rows.splice(index..index, rows);
    }

    pub fn remove(&mut self, id: u64) {
        self.rows.retain(|row| row.id != id);
    }

    /// 移至目标原先的位置：向上拖放插到目标前，向下拖放插到目标后。
    #[cfg(test)]
    pub fn move_to(&mut self, source: u64, target: u64) {
        let source = self.rows.iter().position(|row| row.id == source);
        let target = self.rows.iter().position(|row| row.id == target);
        if let (Some(source), Some(target)) = (source, target) {
            let row = self.rows.remove(source);
            self.rows.insert(target, row);
        }
    }

    /// 实时拖动时放到目标前方，已经处于目标前方时保持不变。
    pub fn move_before(&mut self, source: u64, target: u64) -> bool {
        self.move_relative(source, target, false)
    }

    /// 实时拖动时放到目标后方，已经处于目标后方时保持不变。
    pub fn move_after(&mut self, source: u64, target: u64) -> bool {
        self.move_relative(source, target, true)
    }

    fn move_relative(&mut self, source: u64, target: u64, after: bool) -> bool {
        let Some(source_index) = self.rows.iter().position(|row| row.id == source) else {
            return false;
        };
        let Some(target_index) = self.rows.iter().position(|row| row.id == target) else {
            return false;
        };
        if source_index == target_index {
            return false;
        }
        if (!after && source_index + 1 == target_index)
            || (after && target_index + 1 == source_index)
        {
            return false;
        }
        let row = self.rows.remove(source_index);
        let target_index = if source_index < target_index {
            target_index - 1
        } else {
            target_index
        };
        let insert_index = if after {
            target_index + 1
        } else {
            target_index
        };
        self.rows.insert(insert_index.min(self.rows.len()), row);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(records: &Records<&'static str>) -> Vec<&'static str> {
        records.rows.iter().map(|row| row.value).collect()
    }

    #[test]
    fn 拖动后多选插入跟随条目身份且保持选择顺序() {
        let mut records = Records::default();
        records.insert_after(None, ["a", "b", "c"]);
        let anchor = records.rows[0].id;
        let target = records.rows[2].id;
        records.move_to(anchor, target);
        records.insert_after(Some(anchor), ["新一", "新二"]);
        assert_eq!(values(&records), ["b", "c", "a", "新一", "新二"]);
        records.move_to(target, records.rows[0].id);
        assert_eq!(values(&records), ["c", "b", "a", "新一", "新二"]);
    }

    #[test]
    fn 删除锚点后追加且删除全部后仍可添加并不复用身份() {
        let mut records = Records::default();
        records.insert_after(None, ["a", "b", "c"]);
        let anchor = records.rows[1].id;
        records.remove(anchor);
        records.insert_after(Some(anchor), ["新一", "新二"]);
        assert_eq!(values(&records), ["a", "c", "新一", "新二"]);
        for id in records.rows.iter().map(|row| row.id).collect::<Vec<_>>() {
            records.remove(id);
        }
        records.insert_after(None, ["重新添加"]);
        assert_eq!(values(&records), ["重新添加"]);
        assert!(records.rows[0].id > anchor);
    }

    #[test]
    fn 取消多选或无效拖动保持草稿() {
        let mut records = Records::default();
        records.insert_after(None, ["a", "b"]);
        records.insert_after(Some(0), []);
        records.move_to(0, 99);
        records.move_to(99, 0);
        records.move_to(0, 0);
        assert_eq!(values(&records), ["a", "b"]);
    }

    #[test]
    fn 实时拖动按目标上下半区移动且重复事件不会抖动() {
        let mut records = Records::default();
        records.insert_after(None, ["a", "b", "c", "d"]);
        let a = records.rows[0].id;
        let c = records.rows[2].id;
        assert!(records.move_before(a, c));
        assert_eq!(values(&records), ["b", "a", "c", "d"]);
        assert!(!records.move_before(a, c));
        assert!(records.move_after(a, c));
        assert_eq!(values(&records), ["b", "c", "a", "d"]);
        assert!(!records.move_after(a, c));
        let d = records.rows[3].id;
        assert!(!records.move_before(a, d));
        assert_eq!(values(&records), ["b", "c", "a", "d"]);
    }
}
