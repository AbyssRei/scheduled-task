//! 环境变量与环境文件的逐条编辑、拖动排序和批量添加。

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AppContext as _, AvailableSpace, Bounds, ContentMask, Context, DragMoveEvent, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Point, Render,
    StatefulInteractiveElement as _, Styled, WeakEntity, Window, canvas, div, point, px,
};

use super::{TaskDialogView, feedback::FormField, form::parse_env_text};

pub(super) struct EnvInputs {
    pub key: Entity<InputState>,
    pub value: Entity<TextareaState>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RecordKind {
    Environment,
    File,
}

impl RecordKind {
    fn index(self) -> usize {
        match self {
            Self::Environment => 0,
            Self::File => 1,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct ListGeometry {
    bounds: Bounds<Pixels>,
    visible: Bounds<Pixels>,
}

pub(super) struct ActiveRecordDrag {
    kind: RecordKind,
    id: u64,
    height: Pixels,
}

#[derive(Clone)]
struct DragRecord {
    owner: WeakEntity<TaskDialogView>,
    kind: RecordKind,
    id: u64,
    row_bounds: Rc<Cell<Bounds<Pixels>>>,
    list: Rc<Cell<ListGeometry>>,
}

struct RecordDragPreview {
    record: DragRecord,
    grab_offset: Point<Pixels>,
    height: Pixels,
}

/// 按整行尺寸夹住位置；横向固定在列表内，超高条目由可见区域裁剪。
fn constrained_origin(
    visible: Bounds<Pixels>,
    height: Pixels,
    mouse: Point<Pixels>,
    grab_offset: Point<Pixels>,
) -> Point<Pixels> {
    point(
        visible.origin.x,
        (mouse.y - grab_offset.y).clamp(
            visible.origin.y,
            (visible.bottom() - height).max(visible.origin.y),
        ),
    )
}

impl Render for RecordDragPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let record = &self.record;
        let geometry = record.list.get();
        let row = record.owner.update(cx, |this, cx| {
            this.record_visual(record.kind, record.id, None, cx)
                .w(geometry.bounds.size.width)
                .h(self.height)
                .into_any_element()
        });
        let list = record.list.clone();
        let grab_offset = self.grab_offset;
        let height = self.height;
        let mouse = window.mouse_position();
        let owner = record.owner.clone();
        // GPUI 的默认拖动根节点不受父级裁剪；在这里以列表的实时可见区域绘制。
        let preview = canvas(
            move |_, window, cx| {
                let Ok(mut row) = row else { return None };
                let visible = list.get().visible;
                if visible.size.width <= px(0.) || visible.size.height <= px(0.) {
                    return None;
                }
                row.layout_as_root(AvailableSpace::min_size(), window, cx);
                window.with_content_mask(Some(ContentMask { bounds: visible }), |window| {
                    row.prepaint_at(
                        constrained_origin(visible, height, mouse, grab_offset),
                        window,
                        cx,
                    );
                });
                Some((row, visible))
            },
            |_, row, window, cx| {
                if let Some((mut row, visible)) = row {
                    window.with_content_mask(Some(ContentMask { bounds: visible }), |window| {
                        row.paint(window, cx);
                    });
                }
            },
        )
        .size(px(1.));
        div()
            .capture_key_down(move |event, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_active_drag(window);
                    let _ = owner.update(cx, |this, cx| {
                        this.dragging = None;
                        cx.notify();
                    });
                    cx.stop_propagation();
                }
            })
            .child(preview)
    }
}

impl TaskDialogView {
    pub(super) fn env_inputs(
        key: String,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EnvInputs {
        let key_input = Self::new_input(window, cx, "变量名");
        let value_input = Self::new_textarea(window, cx, "值（可换行，留空表示空值）");
        key_input.update(cx, |state, cx| state.set_value(key, window, cx));
        value_input.update(cx, |state, cx| state.set_value(value, window, cx));
        EnvInputs {
            key: key_input,
            value: value_input,
        }
    }

    pub(super) fn env_file_input(
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let input = Self::new_input(window, cx, ".env 文件路径");
        input.update(cx, |state, cx| state.set_value(path, window, cx));
        input
    }

    fn insert_environment(
        &mut self,
        after: Option<u64>,
        values: Vec<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inputs = values
            .into_iter()
            .map(|(key, value)| Self::env_inputs(key, value, window, cx))
            .collect::<Vec<_>>();
        if let Some(first) = inputs.first() {
            first.key.update(cx, |state, cx| state.focus(window, cx));
        }
        self.env.insert_after(after, inputs);
        self.draft_changed(cx);
    }

    fn add_environment_button(&self, after: Option<u64>, cx: &Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let bulk_view = view.clone();
        DropdownButton::new(("env-add-menu", after.map_or(0, |id| id + 1)))
            .menu_accessibility_label("更多添加方式")
            .button(
                Button::new(("env-add", after.map_or(0, |id| id + 1)))
                    .label("添加")
                    .tooltip(if after.is_some() {
                        "在此条目下方添加环境变量"
                    } else {
                        "在列表末尾添加环境变量"
                    })
                    .on_click(move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.insert_environment(
                                after,
                                vec![(String::new(), String::new())],
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .dropdown_menu(move |menu, _, _| {
                let view = bulk_view.clone();
                menu.item(
                    PopupMenuItem::new("批量添加").on_click(move |_, window, cx| {
                        let _ = view
                            .update(cx, |this, cx| this.open_bulk_environment(after, window, cx));
                    }),
                )
            })
    }

    fn record_invalid(&self, kind: RecordKind, id: u64) -> bool {
        self.error.as_ref().is_some_and(|error| match error.field {
            FormField::EnvironmentKey(index) | FormField::EnvironmentValue(index)
                if kind == RecordKind::Environment =>
            {
                self.env.rows.get(index).is_some_and(|row| row.id == id)
            }
            FormField::EnvironmentFile(index) if kind == RecordKind::File => self
                .env_files
                .rows
                .get(index)
                .is_some_and(|row| row.id == id),
            _ => false,
        })
    }

    // 原行和预览共用此布局，包括输入实体、按钮类型、边框及间距。
    fn record_visual(
        &self,
        kind: RecordKind,
        id: u64,
        drag: Option<DragRecord>,
        cx: &Context<Self>,
    ) -> gpui_kit::Div {
        let view = cx.entity().downgrade();
        let grip = div()
            .id(("record-grip", id))
            .w(px(22.))
            .h(px(32.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .child("≡")
            .when_some(drag, |grip, drag| {
                grip.on_drag(drag, move |drag, _, window, cx| {
                    let bounds = drag.row_bounds.get();
                    let _ = view.update(cx, |this, cx| {
                        this.dragging = Some(ActiveRecordDrag {
                            kind: drag.kind,
                            id: drag.id,
                            height: bounds.size.height,
                        });
                        cx.notify();
                    });
                    cx.new(|_| RecordDragPreview {
                        record: drag.clone(),
                        grab_offset: window.mouse_position() - bounds.origin,
                        height: bounds.size.height,
                    })
                })
            });
        let row = h_flex()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .gap_1()
            .p_1()
            .rounded_md()
            .border_1()
            .border_color(if self.record_invalid(kind, id) {
                cx.theme().danger
            } else {
                cx.theme().border
            })
            .bg(cx.theme().background)
            .text_size(px(14.))
            .text_color(cx.theme().foreground)
            .child(grip);
        match kind {
            RecordKind::Environment => {
                let inputs = &self
                    .env
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .expect("环境变量条目必须存在")
                    .value;
                row.child(Input::new(&inputs.key).w(px(160.)).flex_shrink_0())
                    .child(
                        Textarea::new(&inputs.value)
                            .h(px(environment_value_height(&inputs.value.read(cx).value())))
                            .min_w_0()
                            .flex_1(),
                    )
                    .child(
                        Button::new(("env-delete", id))
                            .label("×")
                            .tooltip("删除此环境变量")
                            .accessibility_label("删除此环境变量")
                            .w(px(28.))
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.env.remove(id);
                                this.draft_changed(cx);
                            })),
                    )
                    .child(self.add_environment_button(Some(id), cx))
            }
            RecordKind::File => {
                let input = &self
                    .env_files
                    .rows
                    .iter()
                    .find(|row| row.id == id)
                    .expect("环境文件条目必须存在")
                    .value;
                row.child(Input::new(input).min_w_0().flex_1())
                    .child(
                        Button::new(("env-file-delete", id))
                            .label("×")
                            .tooltip("删除此环境文件")
                            .accessibility_label("删除此环境文件")
                            .w(px(28.))
                            .ghost()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.env_files.remove(id);
                                this.draft_changed(cx);
                            })),
                    )
                    .child(
                        Button::new(("env-file-add", id))
                            .label("添加...")
                            .tooltip("多选文件，插入此条目下方")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.browse_env_files(Some(id), cx)
                            })),
                    )
            }
        }
    }

    fn record_row(
        &self,
        kind: RecordKind,
        id: u64,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        let owner = cx.entity_id();
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let measure = bounds.clone();
        let list = self.record_lists[kind.index()].clone();
        let drag = DragRecord {
            owner: cx.entity().downgrade(),
            kind,
            id,
            row_bounds: bounds,
            list: list.clone(),
        };
        let active = self
            .dragging
            .as_ref()
            .filter(|drag| drag.kind == kind && drag.id == id);
        let content = if let Some(active) = active {
            // 仅保留等高空位，输入实体只在预览中绘制一次。
            div().w_full().h(active.height).into_any_element()
        } else {
            self.record_visual(kind, id, Some(drag), cx)
                .id(("record-content", id))
                .drag_over::<DragRecord>(move |style, drag, _, cx| {
                    if drag.owner.entity_id() == owner && drag.kind == kind && drag.id != id {
                        style.border_color(cx.theme().primary).bg(cx.theme().muted)
                    } else {
                        style
                    }
                })
                .into_any_element()
        };
        v_flex()
            .id((
                if kind == RecordKind::Environment {
                    "env-row"
                } else {
                    "env-file-row"
                },
                id,
            ))
            .relative()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .anchor_scroll(if self.record_invalid(kind, id) {
                self.error_scroll.clone()
            } else {
                None
            })
            .can_drop(move |value, _, _| {
                value
                    .downcast_ref::<DragRecord>()
                    .is_some_and(|drag| drag.owner.entity_id() == owner && drag.kind == kind)
            })
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DragRecord>, _, cx| {
                    if !event.event.dragging()
                        || !list.get().visible.contains(&event.event.position)
                    {
                        return;
                    }
                    let drag = event.drag(cx);
                    let drag_id = drag.id;
                    if drag.owner.entity_id() != cx.entity_id()
                        || drag.kind != kind
                        || drag_id == id
                        || !event.bounds.contains(&event.event.position)
                    {
                        return;
                    }
                    let before = event.event.position.y < event.bounds.center().y;
                    let changed = match kind {
                        RecordKind::Environment if before => this.env.move_before(drag_id, id),
                        RecordKind::Environment => this.env.move_after(drag_id, id),
                        RecordKind::File if before => this.env_files.move_before(drag_id, id),
                        RecordKind::File => this.env_files.move_after(drag_id, id),
                    };
                    if changed {
                        this.draft_changed(cx);
                    }
                }),
            )
            .on_drop(cx.listener(move |this, drag: &DragRecord, _, cx| {
                if drag.owner.entity_id() == cx.entity_id() && drag.kind == kind {
                    this.dragging = None;
                    cx.notify();
                }
            }))
            .child(content)
            .child(
                canvas(move |bounds, _, _| measure.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
    }

    fn record_list(&self, kind: RecordKind, cx: &Context<Self>) -> impl IntoElement + use<> {
        let geometry = self.record_lists[kind.index()].clone();
        let ids: Vec<_> = match kind {
            RecordKind::Environment => self.env.rows.iter().map(|row| row.id).collect(),
            RecordKind::File => self.env_files.rows.iter().map(|row| row.id).collect(),
        };
        v_flex()
            .relative()
            .w_full()
            .min_w_0()
            .flex_shrink_0()
            .gap_2()
            .children(ids.into_iter().map(|id| self.record_row(kind, id, cx)))
            .child(
                canvas(
                    move |bounds, window, _| {
                        geometry.set(ListGeometry {
                            bounds,
                            visible: bounds.intersect(&window.content_mask().bounds),
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }

    pub(super) fn environment_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_2()
            .w_full()
            .flex_shrink_0()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} 条变量", self.env.rows.len())),
                    )
                    .child(self.add_environment_button(None, cx)),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("拖动 ≡ 排序；值保留空格和换行，留空表示设置为空值。"),
            )
            .when(self.env.rows.is_empty(), |editor| {
                editor.child(
                    div()
                        .p_3()
                        .rounded_md()
                        .bg(cx.theme().muted)
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("暂无显式变量。点击“添加”逐条填写，或从下拉菜单批量导入。"),
                )
            })
            .child(self.record_list(RecordKind::Environment, cx))
    }

    pub(super) fn env_files_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_2()
            .w_full()
            .flex_shrink_0()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} 个文件", self.env_files.rows.len())),
                    )
                    .child(
                        Button::new("env-files-add-empty")
                            .label("添加文件...")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.browse_env_files(None, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().muted_foreground)
                    .child("拖动 ≡ 排序；添加支持多选，行内按钮插入当前条目下方。"),
            )
            .when(self.env_files.rows.is_empty(), |editor| {
                editor.child(
                    div()
                        .p_3()
                        .rounded_md()
                        .bg(cx.theme().muted)
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("暂无环境文件。可添加一个或多个 .env 文件。"),
                )
            })
            .child(self.record_list(RecordKind::File, cx))
    }
    fn open_bulk_environment(
        &mut self,
        after: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            let mut input = TextareaState::new(window, cx);
            input.set_placeholder(
                "每行一个 KEY=VALUE，空行和 # 注释忽略，支持 export KEY=VALUE",
                window,
                cx,
            );
            input
        });
        let owner = cx.entity().downgrade();
        let batch = cx.new(|cx: &mut Context<BatchEnvironment>| {
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            })
            .detach();
            BatchEnvironment {
                owner,
                after,
                input,
                error: None,
            }
        });
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = batch.clone();
            let keyboard_confirm = batch.clone();
            dialog
                .title("批量添加环境变量")
                .w(px(560.))
                .overlay_closable(false)
                .on_ok(move |_, window, cx| {
                    keyboard_confirm.update(cx, |this, cx| this.save(window, cx))
                })
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("env-bulk-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("env-bulk-confirm")
                                .label("添加")
                                .primary()
                                .on_click(move |_, window, cx| {
                                    if confirm.update(cx, |this, cx| this.save(window, cx)) {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
                .child(batch.clone())
        });
    }
}

struct BatchEnvironment {
    owner: WeakEntity<TaskDialogView>,
    after: Option<u64>,
    input: Entity<TextareaState>,
    error: Option<String>,
}

impl BatchEnvironment {
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match parse_env_text(&self.input.read(cx).value()) {
            Ok(values) if values.is_empty() => {
                self.error = Some("请至少填写一条环境变量".into());
                cx.notify();
                false
            }
            Ok(values) => {
                let _ = self.owner.update(cx, |this, cx| {
                    this.insert_environment(self.after, values, window, cx)
                });
                true
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                false
            }
        }
    }
}

impl Render for BatchEnvironment {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(div().child("全部校验通过后按输入顺序插入所选条目下方，取消保留原任务草稿。"))
            .child(
                Textarea::new(&self.input)
                    .h(px(220.))
                    .w_full()
                    .flex_shrink_0(),
            )
            .when_some(self.error.clone(), |layout, error| {
                layout.child(
                    div()
                        .text_size(px(12.))
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
    }
}

fn environment_value_height(value: &str) -> f32 {
    32. + (value.split('\n').count().clamp(1, 4) - 1) as f32 * 20.
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::size;

    fn area() -> Bounds<Pixels> {
        Bounds::new(point(px(100.), px(200.)), size(px(650.), px(300.)))
    }

    #[test]
    fn 拖动保留抓取点偏移并锁定横向位置() {
        let origin = constrained_origin(
            area(),
            px(42.),
            point(px(900.), px(300.)),
            point(px(16.), px(18.)),
        );
        assert_eq!(origin, point(px(100.), px(282.)));
    }

    #[test]
    fn 整行在上下边界处停住() {
        let top = constrained_origin(
            area(),
            px(82.),
            point(px(-500.), px(-500.)),
            point(px(16.), px(20.)),
        );
        let bottom = constrained_origin(
            area(),
            px(82.),
            point(px(5000.), px(5000.)),
            point(px(16.), px(20.)),
        );
        assert_eq!(top, area().origin);
        assert_eq!(bottom.y + px(82.), area().bottom());
        assert_eq!(bottom.x, area().origin.x);
    }

    #[test]
    fn 多行条目高于可见区域时固定顶部供裁剪() {
        let visible = Bounds::new(point(px(100.), px(460.)), size(px(650.), px(40.)));
        assert_eq!(
            constrained_origin(
                visible,
                px(102.),
                point(px(400.), px(490.)),
                point(px(5.), px(50.))
            ),
            visible.origin
        );
    }

    #[test]
    fn 滚动或缩放后使用当前可见边界() {
        let moved = Bounds::new(point(px(120.), px(350.)), size(px(500.), px(100.)));
        let origin = constrained_origin(
            moved,
            px(42.),
            point(px(1000.), px(800.)),
            point(px(16.), px(18.)),
        );
        assert_eq!(origin, point(px(120.), px(408.)));
    }
}
