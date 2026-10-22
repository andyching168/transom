//! Where a menu, popover or input-method list is drawn inside the proxy window
//! it belongs to, and which of them a pointer position lands on.
//!
//! The host mirrors these transient windows with a `parent` (protocol.md §4). A
//! platform whose compositor lets a client place its own windows can give each a
//! window of its own. Wayland does not: a client cannot position a toplevel, so
//! the Linux client draws them **inside the parent's surface**, which also gives
//! the right stacking and an unambiguous hit test for free. This module is that
//! arithmetic, kept free of any windowing code so it is unit tested on any host.
//!
//! All sizes are physical pixels (I-2). A transient window is never resampled:
//! it is cropped, at native scale, from the same shared texture as its parent.

use crate::wire::Rect;

/// One transient window as the layout needs to know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub id: u64,
    /// The window it hangs off: a shared window or another transient (submenu).
    pub parent: Option<u64>,
    /// Its place in the shared texture.
    pub source: Rect,
    /// Set when the menu was opened from the menu strip's item with this index.
    pub menu_index: Option<u32>,
}

/// Where an `Item` ends up in the parent's client area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub id: u64,
    /// Top-left in the client area (always inside it).
    pub x: u32,
    pub y: u32,
    /// The visible size, never larger than the client area.
    pub w: u32,
    pub h: u32,
    /// The crop of the shared texture that fills `w` x `h`: the item's own top-left,
    /// so a window taller than the client area loses its bottom, not its top.
    pub crop: Rect,
}

impl Placed {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x as i32
            && y >= self.y as i32
            && x < (self.x + self.w) as i32
            && y < (self.y + self.h) as i32
    }
}

/// Lay `items` out in a client area of `client` pixels whose shared window is at
/// `root` in the texture. `items` must list a parent before its children (the host
/// announces them in that order). `anchor` gives, for a menu-strip item index, the
/// x of its left edge and the y just below the strip.
///
/// Each item keeps its offset from its parent, so a popup stays exactly where the
/// Mac put it relative to the window; only when that would leave the client area
/// is it slid back inside, and a child follows its parent's slide so a submenu
/// stays next to the menu that opened it.
pub fn layout(
    client: (u32, u32),
    root: Rect,
    items: &[Item],
    anchor: impl Fn(u32) -> Option<(u32, u32)>,
) -> Vec<Placed> {
    let mut placed: Vec<Placed> = Vec::with_capacity(items.len());
    for item in items {
        let w = item.source.w.min(client.0).max(1);
        let h = item.source.h.min(client.1).max(1);
        let natural = match item.parent.and_then(|p| {
            let parent = placed.iter().find(|q| q.id == p)?;
            let origin = items.iter().find(|i| i.id == p)?.source;
            Some((parent.x as i64, parent.y as i64, origin))
        }) {
            Some((px, py, origin)) => (
                px + item.source.x as i64 - origin.x as i64,
                py + item.source.y as i64 - origin.y as i64,
            ),
            None => (
                item.source.x as i64 - root.x as i64,
                item.source.y as i64 - root.y as i64,
            ),
        };
        let (mut x, mut y) = natural;
        if let Some((ax, ay)) = item.menu_index.and_then(&anchor) {
            x = ax as i64;
            y = ay as i64;
        }
        let x = x.clamp(0, (client.0 - w) as i64) as u32;
        let y = y.clamp(0, (client.1 - h) as i64) as u32;
        placed.push(Placed {
            id: item.id,
            x,
            y,
            w,
            h,
            crop: Rect {
                x: item.source.x,
                y: item.source.y,
                w,
                h,
            },
        });
    }
    placed
}

/// The topmost placed window under client pixel `(x, y)`, with the point in that
/// window's own coordinates. Later items are on top.
pub fn hit(placed: &[Placed], x: i32, y: i32) -> Option<(u64, i32, i32)> {
    placed
        .iter()
        .rev()
        .find(|p| p.contains(x, y))
        .map(|p| (p.id, x - p.x as i32, y - p.y as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: Rect = Rect {
        x: 200,
        y: 632,
        w: 520,
        h: 348,
    };

    fn item(id: u64, parent: Option<u64>, x: u32, y: u32, w: u32, h: u32) -> Item {
        Item {
            id,
            parent,
            source: Rect { x, y, w, h },
            menu_index: None,
        }
    }

    fn no_anchor(_: u32) -> Option<(u32, u32)> {
        None
    }

    #[test]
    fn a_popup_inside_the_window_keeps_its_offset() {
        // The NSPopUpButton menu measured on the Mac: (217,698) 200x98 over a
        // window at (200,632).
        let placed = layout((520, 348), ROOT, &[item(3, Some(1), 217, 698, 200, 98)], no_anchor);
        assert_eq!((placed[0].x, placed[0].y, placed[0].w, placed[0].h), (17, 66, 200, 98));
        assert_eq!(placed[0].crop, Rect { x: 217, y: 698, w: 200, h: 98 });
    }

    #[test]
    fn a_popup_hanging_off_the_window_is_slid_back_inside() {
        // Starts left of and above the window: pinned to the top-left corner.
        let placed = layout((520, 348), ROOT, &[item(3, Some(1), 100, 600, 80, 40)], no_anchor);
        assert_eq!((placed[0].x, placed[0].y), (0, 0));
        // Hangs off the bottom right.
        let placed = layout((520, 348), ROOT, &[item(3, Some(1), 700, 960, 80, 40)], no_anchor);
        assert_eq!((placed[0].x, placed[0].y), (440, 308));
    }

    #[test]
    fn a_popup_larger_than_the_window_is_cropped_not_scaled() {
        let placed = layout((300, 200), ROOT, &[item(3, Some(1), 210, 640, 500, 631)], no_anchor);
        assert_eq!((placed[0].w, placed[0].h), (300, 200));
        // The crop starts at the popup's own top-left, so its top stays visible.
        assert_eq!((placed[0].crop.x, placed[0].crop.y), (210, 640));
        assert_eq!((placed[0].crop.w, placed[0].crop.h), (300, 200));
    }

    #[test]
    fn a_menu_from_the_strip_goes_under_its_item() {
        // The Mac put the File menu at the top of the display, nowhere near the
        // window; the strip item decides.
        let mut menu = item(2, Some(1), 162, 25, 55, 32);
        menu.menu_index = Some(2);
        let placed = layout((520, 348), ROOT, &[menu], |i| (i == 2).then_some((90, 28)));
        assert_eq!((placed[0].x, placed[0].y), (90, 28));
    }

    #[test]
    fn a_menu_whose_item_is_unknown_falls_back_to_its_offset() {
        let mut menu = item(2, Some(1), 262, 700, 55, 32);
        menu.menu_index = Some(9);
        let placed = layout((520, 348), ROOT, &[menu], no_anchor);
        assert_eq!((placed[0].x, placed[0].y), (62, 68));
    }

    #[test]
    fn a_submenu_follows_its_menu_when_that_was_slid() {
        let mut menu = item(2, Some(1), 162, 25, 100, 60);
        menu.menu_index = Some(2);
        // The submenu sits 100px right of the menu on the Mac.
        let sub = item(3, Some(2), 262, 30, 100, 60);
        let placed = layout((520, 348), ROOT, &[menu, sub], |_| Some((90, 28)));
        assert_eq!((placed[0].x, placed[0].y), (90, 28));
        assert_eq!((placed[1].x, placed[1].y), (190, 33));
    }

    #[test]
    fn hit_prefers_the_topmost_and_reports_local_coordinates() {
        let placed = layout(
            (520, 348),
            ROOT,
            &[item(2, Some(1), 210, 640, 200, 100), item(3, Some(1), 260, 660, 100, 50)],
            no_anchor,
        );
        // Inside both: the later one is on top.
        assert_eq!(hit(&placed, 70, 40), Some((3, 10, 12)));
        // Inside only the first.
        assert_eq!(hit(&placed, 12, 12), Some((2, 2, 4)));
        // Past the right and bottom edges is outside (exclusive).
        assert_eq!(hit(&placed, 10 + 200, 8), None);
        assert_eq!(hit(&placed, 12, 8 + 100), None);
        assert_eq!(hit(&placed, -1, 10), None);
    }
}
