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
    /// How far into the window the visible part starts (nonzero when its top or
    /// left was clipped), and the whole window's size: a pointer's local point is
    /// reported in the whole window's coordinates, which is what the host maps.
    pub off: (u32, u32),
    pub full: (u32, u32),
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
/// **Where the Mac drew a popup decides how it is shown**, because the window's own
/// crop of the shared texture already contains whatever the popup covers:
///
/// - If any of it lies over the window, it is drawn exactly there (clipped to the
///   client area). Moving it would show the same pixels twice at two offsets, the
///   copy in the crop included.
/// - If it lies wholly outside the window (a menu under the Mac's menu bar, a
///   candidate list hanging off an edge) nothing of it is in the crop, so it is
///   put where it can be seen: under its menu-strip item, or slid back inside.
///
/// A child (submenu) keeps its offset from its parent's unclipped position.
pub fn layout(
    client: (u32, u32),
    root: Rect,
    items: &[Item],
    anchor: impl Fn(u32) -> Option<(u32, u32)>,
) -> Vec<Placed> {
    let (cw, ch) = (client.0 as i64, client.1 as i64);
    let mut placed: Vec<Placed> = Vec::with_capacity(items.len());
    // Each item's unclipped top-left in the client area, for its children.
    let mut origins: Vec<(u64, i64, i64)> = Vec::with_capacity(items.len());
    for item in items {
        let (w, h) = (item.source.w.max(1) as i64, item.source.h.max(1) as i64);
        let (nx, ny) = match item.parent.and_then(|p| {
            let (_, ox, oy) = *origins.iter().find(|o| o.0 == p)?;
            let parent = items.iter().find(|i| i.id == p)?.source;
            Some((ox, oy, parent))
        }) {
            Some((ox, oy, parent)) => (
                ox + item.source.x as i64 - parent.x as i64,
                oy + item.source.y as i64 - parent.y as i64,
            ),
            None => (
                item.source.x as i64 - root.x as i64,
                item.source.y as i64 - root.y as i64,
            ),
        };
        let (x0, y0) = (nx.max(0), ny.max(0));
        let (x1, y1) = ((nx + w).min(cw), (ny + h).min(ch));
        if x1 > x0 && y1 > y0 {
            origins.push((item.id, nx, ny));
            placed.push(Placed {
                id: item.id,
                x: x0 as u32,
                y: y0 as u32,
                w: (x1 - x0) as u32,
                h: (y1 - y0) as u32,
                crop: Rect {
                    x: item.source.x + (x0 - nx) as u32,
                    y: item.source.y + (y0 - ny) as u32,
                    w: (x1 - x0) as u32,
                    h: (y1 - y0) as u32,
                },
                off: ((x0 - nx) as u32, (y0 - ny) as u32),
                full: (item.source.w, item.source.h),
            });
            continue;
        }
        // Wholly outside: nothing of it is in the window's crop.
        let (mut x, mut y) = (nx, ny);
        if let Some((ax, ay)) = item.menu_index.and_then(&anchor) {
            x = ax as i64;
            y = ay as i64;
        }
        let vw = w.min(cw).max(1);
        let vh = h.min(ch).max(1);
        let x = x.clamp(0, cw - vw);
        let y = y.clamp(0, ch - vh);
        origins.push((item.id, x, y));
        placed.push(Placed {
            id: item.id,
            x: x as u32,
            y: y as u32,
            w: vw as u32,
            h: vh as u32,
            crop: Rect {
                x: item.source.x,
                y: item.source.y,
                w: vw as u32,
                h: vh as u32,
            },
            off: (0, 0),
            full: (item.source.w, item.source.h),
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
        .map(|p| {
            (
                p.id,
                x - p.x as i32 + p.off.0 as i32,
                y - p.y as i32 + p.off.1 as i32,
            )
        })
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
    fn a_popup_partly_over_the_window_stays_where_the_mac_drew_it_clipped() {
        // Starts left of and above the window: only the part over it is shown, at
        // the same place the crop already has those pixels.
        let placed = layout((520, 348), ROOT, &[item(3, Some(1), 180, 600, 100, 100)], no_anchor);
        assert_eq!((placed[0].x, placed[0].y, placed[0].w, placed[0].h), (0, 0, 80, 68));
        assert_eq!((placed[0].crop.x, placed[0].crop.y), (200, 632));
        // Hangs off the bottom right.
        let placed = layout((520, 348), ROOT, &[item(3, Some(1), 700, 960, 80, 40)], no_anchor);
        assert_eq!((placed[0].x, placed[0].y, placed[0].w, placed[0].h), (500, 328, 20, 20));
    }

    #[test]
    fn a_popup_wholly_outside_the_window_is_slid_back_inside() {
        let placed = layout((520, 348), ROOT, &[item(3, Some(1), 100, 500, 80, 40)], no_anchor);
        assert_eq!((placed[0].x, placed[0].y, placed[0].w, placed[0].h), (0, 0, 80, 40));
        assert_eq!((placed[0].crop.x, placed[0].crop.y), (100, 500));
    }

    #[test]
    fn a_menu_over_the_window_is_not_moved_to_its_strip_item() {
        // The reported bug: a large window covers where the Mac drew the menu, so
        // the crop already holds the menu. Anchoring it elsewhere showed it twice.
        let mut menu = item(2, Some(1), 262, 700, 165, 300);
        menu.menu_index = Some(2);
        let placed = layout((520, 348), ROOT, &[menu], |_| Some((90, 28)));
        assert_eq!((placed[0].x, placed[0].y), (62, 68));
    }

    #[test]
    fn a_popup_larger_than_the_window_is_cropped_not_scaled() {
        let placed = layout((300, 200), ROOT, &[item(3, Some(1), 210, 640, 500, 631)], no_anchor);
        assert_eq!((placed[0].x, placed[0].y, placed[0].w, placed[0].h), (10, 8, 290, 192));
        assert_eq!((placed[0].crop.x, placed[0].crop.y), (210, 640));
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
    fn a_hit_on_a_clipped_popup_is_in_the_whole_windows_coordinates() {
        // 20px of the popup's left edge hangs off the window: its visible part
        // starts at client x 0 but is the popup's x 20.
        let placed = layout((520, 348), ROOT, &[item(3, Some(1), 180, 700, 100, 100)], no_anchor);
        assert_eq!(placed[0].off, (20, 0));
        assert_eq!(hit(&placed, 5, 70), Some((3, 25, 2)));
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
