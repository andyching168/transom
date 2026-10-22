import ApplicationServices
import CoreGraphics
import Foundation

/// Mints and owns the opaque `u64` window identity the wire uses (protocol.md §3),
/// and the mapping to the `AXUIElement` it stands for.
///
/// The client never interprets the id; it is just a token it keys its proxy
/// windows on. Keeping the AX↔id correlation confined to this type is exactly
/// what protocol.md asks for: however OQ-3 is eventually resolved (private
/// `_AXUIElementGetWindow`, frame heuristics, …), it stays behind this wall.
///
/// Thread-safe: the AX watcher mutates it from a run-loop thread while the control
/// server reads snapshots from an async task, so every access is under one lock.
public final class WindowRegistry: @unchecked Sendable {

    public struct Entry: Sendable, Equatable {
        public let id: UInt64
        public var rect: WireRect
        public var title: String
        /// `.normal` for a shared AX window; anything else is a transient window
        /// (menu, popover, input-method candidate list) the host tracks through
        /// the WindowServer instead of AX (`AuxWindowTracker`).
        public var kind: WindowKind = .normal
        /// For a transient window: the shared window it is placed against.
        public var parent: UInt64?
        /// For a transient window opened from a menu-bar item: that item's index.
        public var menuIndex: UInt32?
        /// The process that owns the window. Known for every transient window (it
        /// is the receiver of a click on it, and for an input method it is not a
        /// shared app) and for shared windows once the watcher records it.
        public var ownerPID: pid_t?
    }

    /// `AXUIElement` is a CF type; wrap it so it is a dictionary key via
    /// `CFEqual`/`CFHash` regardless of whether the SDK bridges `Hashable`.
    private struct ElementKey: Hashable {
        let element: AXUIElement
        static func == (a: ElementKey, b: ElementKey) -> Bool { CFEqual(a.element, b.element) }
        func hash(into hasher: inout Hasher) { hasher.combine(CFHash(element)) }
    }

    private let lock = NSLock()
    private var nextID: UInt64 = 1
    private var idByElement: [ElementKey: UInt64] = [:]
    private var elementByID: [UInt64: AXUIElement] = [:]
    private var entries: [UInt64: Entry] = [:]
    private var idByAuxWindow: [CGWindowID: UInt64] = [:]
    private var menuBars: [UInt64: [WireMenuItem]] = [:]

    public init() {}

    /// The id for this element, minting a fresh one the first time it is seen.
    public func id(for element: AXUIElement) -> (id: UInt64, isNew: Bool) {
        lock.withLock {
            let key = ElementKey(element: element)
            if let existing = idByElement[key] { return (existing, false) }
            let id = nextID
            nextID += 1
            idByElement[key] = id
            elementByID[id] = element
            return (id, true)
        }
    }

    /// The AX element behind an id, for driving AX writes (Phase 4) and input
    /// injection / window raising (Phase 5).
    public func element(for id: UInt64) -> AXUIElement? {
        lock.withLock { elementByID[id] }
    }

    public func existingID(for element: AXUIElement) -> UInt64? {
        lock.withLock { idByElement[ElementKey(element: element)] }
    }

    /// The current recorded state for an id (its VDS rect + title), or nil if the
    /// id is unknown. Input injection reads the rect to translate a window-local
    /// click into VDS/AX space (issue #7).
    public func entry(for id: UInt64) -> Entry? {
        lock.withLock { entries[id] }
    }

    public func record(id: UInt64, rect: WireRect, title: String, ownerPID: pid_t? = nil) {
        lock.withLock {
            entries[id] = Entry(
                id: id, rect: rect, title: title, ownerPID: ownerPID ?? entries[id]?.ownerPID)
        }
    }

    // MARK: Transient windows

    /// The id for a transient WindowServer window, minting one the first time it is
    /// seen. Ids come from the same counter as AX windows, so they are never reused
    /// and never collide (protocol.md §3).
    public func auxID(for windowID: CGWindowID) -> (id: UInt64, isNew: Bool) {
        lock.withLock {
            if let existing = idByAuxWindow[windowID] { return (existing, false) }
            let id = nextID
            nextID += 1
            idByAuxWindow[windowID] = id
            return (id, true)
        }
    }

    public func recordAux(
        id: UInt64, rect: WireRect, kind: WindowKind, parent: UInt64?, menuIndex: UInt32?,
        ownerPID: pid_t
    ) {
        lock.withLock {
            entries[id] = Entry(
                id: id, rect: rect, title: "", kind: kind, parent: parent, menuIndex: menuIndex,
                ownerPID: ownerPID)
        }
    }

    public func removeAux(windowID: CGWindowID) -> UInt64? {
        lock.withLock {
            guard let id = idByAuxWindow.removeValue(forKey: windowID) else { return nil }
            entries[id] = nil
            return id
        }
    }

    /// Transient windows, id-ordered.
    public func auxSnapshot() -> [Entry] {
        lock.withLock { entries.values.filter { $0.kind != .normal }.sorted { $0.id < $1.id } }
    }

    // MARK: Menu bars

    public func menuBar(for id: UInt64) -> [WireMenuItem]? {
        lock.withLock { menuBars[id] }
    }

    public func setMenuBar(id: UInt64, items: [WireMenuItem]) {
        lock.withLock { menuBars[id] = items }
    }

    public func updateRect(id: UInt64, rect: WireRect) {
        lock.withLock { entries[id]?.rect = rect }
    }

    public func updateTitle(id: UInt64, title: String) {
        lock.withLock { entries[id]?.title = title }
    }

    /// Forget an element (on destroy). Returns its id if it was known.
    public func remove(element: AXUIElement) -> UInt64? {
        lock.withLock {
            let key = ElementKey(element: element)
            guard let id = idByElement[key] else { return nil }
            idByElement[key] = nil
            elementByID[id] = nil
            entries[id] = nil
            menuBars[id] = nil
            return id
        }
    }

    /// Every live **shared** window's current state, id-ordered — the payload for a
    /// fresh client's resync and the set the tiler reasons about (I-5). Transient
    /// windows are deliberately absent: they are placed by their app, not by us
    /// (`auxSnapshot()`).
    public func snapshot() -> [Entry] {
        lock.withLock { entries.values.filter { $0.kind == .normal }.sorted { $0.id < $1.id } }
    }
}
