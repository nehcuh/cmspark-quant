//! Design tokens and small composition helpers for the dashboard.
//!
//! Every colour on screen comes from the active gpui-component `Theme` (shadcn-style
//! tokens); nothing in the views may use a raw colour literal. See docs/UI_DESIGN.md.
//! The theme is a GPUI global, but many view helpers are plain functions without a
//! context, so `sync` snapshots the tokens once per frame (UI thread only).
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::*;
use std::cell::Cell;

#[derive(Clone, Copy)]
pub struct Tokens {
    pub bg: Hsla,
    pub panel: Hsla,
    pub border: Hsla,
    pub fg: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub ok: Hsla,
    pub err: Hsla,
    pub warn: Hsla,
    pub caution: Hsla,
    pub vault: Hsla,
    pub cyan: Hsla,
    pub hover: Hsla,
    pub active: Hsla,
    pub sunken: Hsla,
}

thread_local! {
    static TOK: Cell<Option<Tokens>> = const { Cell::new(None) };
}

/// Snapshot the active theme's tokens; call at the top of every root render.
pub fn sync(cx: &App) {
    let t = cx.theme();
    TOK.with(|c| {
        c.set(Some(Tokens {
            bg: t.background,
            panel: t.group_box,
            border: t.border,
            fg: t.foreground,
            muted: t.muted_foreground,
            accent: t.blue,
            ok: t.success,
            err: t.danger,
            warn: t.yellow,
            caution: t.warning,
            vault: t.magenta,
            cyan: t.cyan,
            hover: t.list_hover,
            active: t.list_active,
            sunken: t.muted,
        }))
    });
}

pub fn tok() -> Tokens {
    TOK.with(|c| c.get())
        .expect("ui::sync must run before rendering")
}

/// Soft background tint of a semantic colour (status rows, callouts).
pub fn tint(c: Hsla) -> Hsla {
    c.opacity(0.12)
}

/// Status / label chip: gpui-component `Tag`, outlined in the given semantic colour.
pub fn chip(text: impl Into<SharedString>, color: Hsla) -> Tag {
    Tag::custom(tint(color), color, color)
        .outline()
        .small()
        .child(text.into())
}

/// Callout box (bordered, tinted) in a semantic colour.
pub fn callout(color: Hsla) -> Div {
    div()
        .p_3()
        .rounded_md()
        .border_1()
        .border_color(color)
        .bg(tint(color))
        .text_sm()
        .text_color(color)
}
