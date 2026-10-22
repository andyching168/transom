import ApplicationServices
import Foundation

// The macOS menu bar, for a client that has no global menu bar of its own.
//
// The menu bar is drawn by the WindowServer ("Window Server", layer 24, measured
// on the Mac), so it is not in the app-filtered capture; and even in a whole
// display capture it would show the menus of whichever app is *really*
// frontmost, while `InputDelivery.background` deliberately leaves that alone.
//
// The accessibility tree does not have that problem: an app's `AXMenuBar` is
// readable while the app is in the background (measured: Finder's, with Claude
// frontmost), and `AXPress` on one of its items opens the real menu without
// changing the frontmost app. So the host sends the *titles* as structured data
// (`menuBar`), the client draws them as its own strip over the proxy, and a click
// on one comes back as `menuPress`. The menu that opens is an ordinary transient
// window (`AuxWindowTracker`), clickable like any other.

/// The AX half, kept apart from the polling so it can be exercised directly.
public enum MenuBar {

    /// Titles of an app's top-level menus, minus the Apple menu (the first item of
    /// every app's menu bar: system-wide, not the shared app's, and it offers
    /// Shut Down). `index` is the item's position in the AX menu bar, which is what
    /// `press` takes back.
    public static func items(pid: pid_t) -> [WireMenuItem]? {
        guard let children = barChildren(pid: pid) else { return nil }
        return titles(of: children.map { child in
            (string(child, kAXTitleAttribute), bool(child, kAXEnabledAttribute) ?? true)
        })
    }

    /// Pure: AX (title, enabled) pairs → wire items.
    static func titles(of entries: [(String?, Bool)]) -> [WireMenuItem] {
        entries.enumerated().compactMap { index, entry in
            guard index > 0, let title = entry.0, !title.isEmpty else { return nil }
            return WireMenuItem(index: UInt32(index), title: title, enabled: entry.1)
        }
    }

    public enum PressOutcome: Equatable, Sendable {
        case opened
        case closed
        case failed(String)
    }

    /// Open menu `index`, or close it if it is the one already open. Pressing a
    /// different item while one is open switches to it (measured), but pressing the
    /// open one again leaves it open, so that case is a cancel.
    public static func press(pid: pid_t, index: Int) -> PressOutcome {
        guard index > 0, let children = barChildren(pid: pid), index < children.count else {
            return .failed("no menu bar item \(index)")
        }
        let item = children[index]
        if bool(item, "AXSelected") == true {
            let result = AXUIElementPerformAction(item, kAXCancelAction as CFString)
            return result == .success ? .closed : .failed("AXCancel \(result.rawValue)")
        }
        let result = AXUIElementPerformAction(item, kAXPressAction as CFString)
        return result == .success ? .opened : .failed("AXPress \(result.rawValue)")
    }

    private static func barChildren(pid: pid_t) -> [AXUIElement]? {
        let app = AXUIElementCreateApplication(pid)
        // A beachballing app must not stall the poll that serves every other app.
        AXUIElementSetMessagingTimeout(app, 0.4)
        var bar: CFTypeRef?
        guard AXUIElementCopyAttributeValue(app, kAXMenuBarAttribute as CFString, &bar) == .success,
            let bar, CFGetTypeID(bar) == AXUIElementGetTypeID()
        else { return nil }
        var children: CFTypeRef?
        guard
            AXUIElementCopyAttributeValue(
                unsafeDowncast(bar, to: AXUIElement.self), kAXChildrenAttribute as CFString,
                &children) == .success,
            let list = children as? [AXUIElement]
        else { return nil }
        return list
    }

    private static func string(_ element: AXUIElement, _ attribute: String) -> String? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute as CFString, &value) == .success
        else { return nil }
        return value as? String
    }

    private static func bool(_ element: AXUIElement, _ attribute: String) -> Bool? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute as CFString, &value) == .success
        else { return nil }
        return (value as? NSNumber)?.boolValue
    }
}

/// Keeps every shared window's menu bar current and carries out presses.
///
/// `@unchecked Sendable`: state is behind `lock`; `refresh()` and `press` may be
/// called from different tasks.
public final class MenuBarService: @unchecked Sendable {
    private let registry: WindowRegistry
    private let emit: @Sendable (WindowWatcher.WindowEvent) -> Void

    private struct Pressed {
        var press: AuxParentResolver.Press
        var time: TimeInterval
    }
    private let lock = NSLock()
    private var pressed: [pid_t: Pressed] = [:]

    /// A menu that opens later than this after a press is not that press's menu.
    static let pressWindow: TimeInterval = 3

    public init(
        registry: WindowRegistry, emit: @escaping @Sendable (WindowWatcher.WindowEvent) -> Void
    ) {
        self.registry = registry
        self.emit = emit
    }

    /// The press, if any, a menu of `pid` opening now should be anchored to.
    public func recentPress(pid: pid_t) -> AuxParentResolver.Press? {
        lock.withLock {
            guard let p = pressed[pid],
                ProcessInfo.processInfo.systemUptime - p.time <= Self.pressWindow
            else { return nil }
            return p.press
        }
    }

    /// The menu that just opened used the press; a later, unrelated popup must not
    /// be anchored to it.
    public func consumePress(pid: pid_t) {
        lock.withLock { pressed[pid] = nil }
    }

    /// Re-read each shared app's menu bar and send what changed. Reads are per app,
    /// not per window: every window of an app shares its menu bar.
    public func refresh() {
        let windows = registry.snapshot()
        var byPID: [pid_t: [WindowRegistry.Entry]] = [:]
        for window in windows {
            guard let pid = window.ownerPID else { continue }
            byPID[pid, default: []].append(window)
        }
        for (pid, entries) in byPID {
            guard let items = MenuBar.items(pid: pid) else { continue }
            for entry in entries where registry.menuBar(for: entry.id) != items {
                registry.setMenuBar(id: entry.id, items: items)
                emit(.menuBar(id: entry.id, items: items))
            }
        }
    }

    /// Carry out the client's `menuPress`.
    @discardableResult
    public func press(id: UInt64, index: UInt32) -> MenuBar.PressOutcome {
        guard let entry = registry.entry(for: id), entry.kind == .normal, let pid = entry.ownerPID
        else { return .failed("unknown window \(id)") }
        lock.withLock {
            pressed[pid] = Pressed(
                press: .init(windowID: id, index: index),
                time: ProcessInfo.processInfo.systemUptime)
        }
        let outcome = MenuBar.press(pid: pid, index: Int(index))
        if outcome != .opened { consumePress(pid: pid) }
        return outcome
    }
}
