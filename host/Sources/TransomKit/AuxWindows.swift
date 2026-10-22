import ApplicationServices
import CoreGraphics
import Darwin
import Foundation

// Transient ("aux") windows: menus, popovers, panels and input-method candidate
// lists.
//
// The shared AX windows are only part of what a person sees on the Mac. Every
// drop-down, context menu, Office gallery and IME candidate list is a *separate*
// WindowServer window that the app (or the input method) places wherever it
// likes. The display capture already contains the ones owned by shared apps, but
// the client crops per-window rects, so a popup that overhangs its window, or sits
// in the gutter, or above the menu bar, was never on screen on the PC. And the
// ones owned by an input method were not even in the capture: it is filtered to
// the shared apps (architecture.md §8, "Selected apps must be filtered").
//
// So the host reports each of them as its own window with a `kind` and a
// `parent`, and the client gives it a borderless, topmost, non-activating window
// anchored to the parent's proxy. Pixels still come from the one shared capture;
// nothing is re-captured or resampled (I-1).

/// A transient window that should be mirrored to the client.
public struct AuxCandidate: Sendable, Equatable {
    public let windowID: CGWindowID
    public let pid: pid_t
    public let layer: Int
    public let kind: WindowKind
    /// Where it is in the capture: VDS physical pixels, clipped to the display.
    public let rect: WireRect
    /// Owned by an input method rather than by a shared app.
    public let isInputMethod: Bool
}

public enum AuxWindowClassifier {

    /// `NSPopUpMenuWindowLevel`: what a native `NSMenu` is drawn at (measured on
    /// the Mac: a Finder menu reports layer 101).
    public static let popUpMenuLayer = 101
    /// The WindowServer's own menu bar and the status-item strip. Neither is an
    /// app window, and neither can be in the app-filtered capture.
    static let menuBarLayer = 24
    static let statusItemLayer = 25
    /// Above this are drag images, the cursor and accessibility overlays.
    static let maximumLayer = 500
    /// Smaller than this (points) is a shadow or a hit-test helper, not UI.
    static let minimumPoints: CGFloat = 4
    static let frameTolerance: CGFloat = 2
    /// A sane bound on how many one app can put in front of a client.
    static let maximumCount = 16

    /// Pick the transient windows out of a front-to-back WindowServer list.
    ///
    /// - Parameters:
    ///   - sharedPIDs: apps being shared.
    ///   - inputMethodPIDs: input-method processes (`InputMethodProcesses`).
    ///   - trackedFrames: AX frames (points) of the shared windows already mirrored
    ///     as normal windows, so a floating panel that AX tracks is not reported
    ///     twice.
    public static func classify(
        _ windows: [WindowServerWindow], sharedPIDs: Set<pid_t>, inputMethodPIDs: Set<pid_t>,
        trackedFrames: [CGRect], display: DisplayInfo
    ) -> [AuxCandidate] {
        var out: [AuxCandidate] = []
        for window in windows {
            let isInputMethod = inputMethodPIDs.contains(window.pid)
            guard isInputMethod || sharedPIDs.contains(window.pid) else { continue }
            guard window.alpha > 0, window.layer > 0, window.layer < maximumLayer,
                window.layer != menuBarLayer,
                window.bounds.width >= minimumPoints, window.bounds.height >= minimumPoints
            else { continue }
            // A shared app's status items live at the status layer; an input
            // method has none, and a palette it draws may well sit there.
            if !isInputMethod && window.layer == statusItemLayer { continue }
            if !isInputMethod, trackedFrames.contains(where: { isSame($0, window.bounds) }) {
                continue
            }
            guard let rect = captureRect(window.bounds, display: display) else { continue }
            let kind: WindowKind =
                !isInputMethod && window.layer == popUpMenuLayer ? .menu : .popover
            out.append(
                AuxCandidate(
                    windowID: window.id, pid: window.pid, layer: window.layer, kind: kind,
                    rect: rect, isInputMethod: isInputMethod))
            if out.count == maximumCount { break }
        }
        return out
    }

    /// A WindowServer frame (AX global points) as the part of it that is inside the
    /// capture, in VDS pixels. A popup may legitimately hang off the display edge,
    /// so it is clipped rather than refused (a normal window is refused,
    /// `WindowWatcher.captureRect`, because its crop must be exact). Clipping keeps
    /// the origin consistent: a click's window-local point is added to the clipped
    /// origin, which is where those pixels really are.
    public static func captureRect(_ frame: CGRect, display: DisplayInfo) -> WireRect? {
        let pixels = Coordinates.displayPixels(
            fromAXRect: frame, displayOriginPoints: display.originPoints, scale: display.scale)
        guard [pixels.minX, pixels.minY, pixels.width, pixels.height].allSatisfy(\.isFinite) else {
            return nil
        }
        let clipped = pixels.intersection(
            CGRect(x: 0, y: 0, width: display.pixelWidth, height: display.pixelHeight))
        guard !clipped.isNull, clipped.width >= 1, clipped.height >= 1 else { return nil }
        let rect = WireRect(clampingVDSPixels: clipped)
        return rect.w > 0 && rect.h > 0 ? rect : nil
    }

    static func isSame(_ a: CGRect, _ b: CGRect) -> Bool {
        abs(a.minX - b.minX) <= frameTolerance && abs(a.minY - b.minY) <= frameTolerance
            && abs(a.width - b.width) <= frameTolerance
            && abs(a.height - b.height) <= frameTolerance
    }
}

/// Which shared window a transient window is placed against on the client.
public enum AuxParentResolver {

    /// A menu-bar item the client pressed a moment ago.
    public struct Press: Sendable, Equatable {
        public var windowID: UInt64
        public var index: UInt32
        public init(windowID: UInt64, index: UInt32) {
            self.windowID = windowID
            self.index = index
        }
    }

    public struct Result: Sendable, Equatable {
        public var parent: UInt64?
        public var menuIndex: UInt32?
    }

    /// - Parameters:
    ///   - candidate: the new transient window.
    ///   - shared: the shared windows (`WindowRegistry.snapshot()`).
    ///   - openMenus: transient menus of the same process that are already open,
    ///     oldest first. A menu opened while another is open is its submenu.
    ///   - press: the fresh menu-bar press for this process, if any.
    ///   - focused: the window the client last focused.
    public static func resolve(
        candidate: AuxCandidate, shared: [WindowRegistry.Entry],
        openMenus: [WindowRegistry.Entry], press: Press?, focused: UInt64?
    ) -> Result {
        if candidate.kind == .menu {
            // A submenu hangs off the menu that is already open.
            if let open = openMenus.last { return Result(parent: open.id, menuIndex: nil) }
            // A top-level menu opened from the client's menu strip.
            if let press, shared.contains(where: { $0.id == press.windowID }) {
                return Result(parent: press.windowID, menuIndex: press.index)
            }
        }
        return Result(
            parent: placementWindow(for: candidate, shared: shared, focused: focused),
            menuIndex: nil)
    }

    /// The shared window the transient window sits on top of: of the windows it
    /// overlaps, the one it overlaps most (preferring its own app's); with no
    /// overlap, the focused window; failing that, the first.
    static func placementWindow(
        for candidate: AuxCandidate, shared: [WindowRegistry.Entry], focused: UInt64?
    ) -> UInt64? {
        func area(_ a: WireRect, _ b: WireRect) -> Int64 {
            let w = min(Int64(a.x) + Int64(a.w), Int64(b.x) + Int64(b.w)) - max(Int64(a.x), Int64(b.x))
            let h = min(Int64(a.y) + Int64(a.h), Int64(b.y) + Int64(b.h)) - max(Int64(a.y), Int64(b.y))
            return w > 0 && h > 0 ? w * h : 0
        }
        let overlapping = shared.compactMap { entry -> (WindowRegistry.Entry, Int64)? in
            let overlap = area(entry.rect, candidate.rect)
            return overlap > 0 ? (entry, overlap) : nil
        }
        if let best = overlapping.max(by: { lhs, rhs in
            let l = (lhs.0.ownerPID == candidate.pid ? 1 : 0, lhs.1)
            let r = (rhs.0.ownerPID == candidate.pid ? 1 : 0, rhs.1)
            return l < r
        }) {
            return best.0.id
        }
        if let focused, shared.contains(where: { $0.id == focused }) { return focused }
        return shared.first?.id
    }
}

/// Input-method processes: the apps whose candidate lists and palettes the person
/// is looking at while they type into a shared app.
public enum InputMethodProcesses {

    /// Third-party input methods live in `Input Methods` folders; the system's
    /// candidate UI is a helper XPC service (`CursorUIViewService`).
    public static func isInputMethod(executablePath path: String) -> Bool {
        path.contains("/Input Methods/") || path.contains("/TextInputUIMacHelper.framework/")
    }

    /// Every running process whose executable looks like an input method.
    public static func scan() -> Set<pid_t> {
        let count = proc_listallpids(nil, 0)
        guard count > 0 else { return [] }
        var pids = [pid_t](repeating: 0, count: Int(count) + 64)
        let filled = proc_listallpids(&pids, Int32(pids.count * MemoryLayout<pid_t>.stride))
        guard filled > 0 else { return [] }
        var path = [CChar](repeating: 0, count: 4 * Int(MAXPATHLEN))
        var found: Set<pid_t> = []
        for pid in pids.prefix(Int(filled)) where pid > 0 {
            guard proc_pidpath(pid, &path, UInt32(path.count)) > 0 else { continue }
            if isInputMethod(executablePath: String(decoding: path.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)) {
                found.insert(pid)
            }
        }
        return found
    }
}

/// Watches the WindowServer for transient windows and mirrors them into the
/// registry and the event stream (`.auxCreated`, then `.moved` / `.destroyed`).
///
/// Polled rather than notified: the app's menu windows fire no AX window
/// notifications (architecture.md OQ-1), and an input method has no AX tree we
/// can observe at all. `CGWindowListCopyWindowInfo` is cheap enough at 25 Hz, and
/// a menu is on screen for seconds.
///
/// `@unchecked Sendable`: the mutable state is behind `lock`; `poll()` is meant to
/// be called from one task at a time.
public final class AuxWindowTracker: @unchecked Sendable {
    private let display: DisplayInfo
    private let registry: WindowRegistry
    private let sharedPIDs: Set<pid_t>
    private let emit: @Sendable (WindowWatcher.WindowEvent) -> Void

    /// The most recent menu-bar press for a process, so the menu it opens is
    /// anchored to the client's strip item (`MenuBarService.recentPress`).
    public var recentPress: (@Sendable (pid_t) -> AuxParentResolver.Press?)?
    /// Called once the menu that `recentPress` described has been anchored to it.
    public var consumePress: (@Sendable (pid_t) -> Void)?
    /// Called when the last open menu goes away (the input layer gives the Mac
    /// its cursor back).
    public var onMenusClosed: (@Sendable () -> Void)?
    /// The client's focused window, the fallback parent.
    public var focusedWindow: (@Sendable () -> UInt64?)?
    /// Called when the set of input-method processes changes, so the capture can
    /// include their windows.
    public var onInputMethodPIDsChanged: (@Sendable (Set<pid_t>) -> Void)?

    private let lock = NSLock()
    private var known: [CGWindowID: (id: UInt64, rect: WireRect)] = [:]
    private var inputMethodPIDs: Set<pid_t> = []
    private var lastScan: TimeInterval = 0
    private var activeUntil: TimeInterval = 0

    /// How often the process table is rescanned for new input methods.
    static let scanInterval: TimeInterval = 2

    public init(
        display: DisplayInfo, registry: WindowRegistry, sharedPIDs: Set<pid_t>,
        emit: @escaping @Sendable (WindowWatcher.WindowEvent) -> Void
    ) {
        self.display = display
        self.registry = registry
        self.sharedPIDs = sharedPIDs
        self.emit = emit
    }

    /// Rescan input-method processes if due. Separate from `poll()` so it can run
    /// once up front: the capture must include an input method *before* its first
    /// candidate list appears.
    @discardableResult
    public func refreshInputMethods(force: Bool = false) -> Set<pid_t> {
        let now = ProcessInfo.processInfo.systemUptime
        let (due, previous) = lock.withLock { (force || now - lastScan >= Self.scanInterval, inputMethodPIDs) }
        guard due else { return previous }
        let scanned = InputMethodProcesses.scan().subtracting(sharedPIDs)
        lock.withLock {
            lastScan = now
            inputMethodPIDs = scanned
        }
        if scanned != previous { onInputMethodPIDsChanged?(scanned) }
        return scanned
    }

    /// How long to sleep before the next `poll()`. A sweep costs ~3 ms (measured), so
    /// it runs every 40 ms while nothing is happening, and every 8 ms while a menu
    /// is open or the client has just pressed something, when a submenu appearing a
    /// frame late is what the person notices.
    public var pollInterval: TimeInterval {
        let now = ProcessInfo.processInfo.systemUptime
        let active = lock.withLock { now < activeUntil } || !registry.auxSnapshot().isEmpty
        return active ? 0.008 : 0.04
    }

    /// The client pressed or typed something: a menu or candidate list may be about
    /// to open, so poll quickly for a moment.
    public func noteActivity() {
        let until = ProcessInfo.processInfo.systemUptime + 1.5
        lock.withLock { activeUntil = until }
    }

    /// One sweep: classify what is on screen now and emit the difference.
    public func poll() {
        let imPIDs = refreshInputMethods()
        let shared = registry.snapshot()
        let tracked = shared.map {
            Coordinates.axGlobalRect(
                fromDisplayPixels: CGRect(
                    x: CGFloat($0.rect.x), y: CGFloat($0.rect.y), width: CGFloat($0.rect.w),
                    height: CGFloat($0.rect.h)),
                displayOriginPoints: display.originPoints, scale: display.scale)
        }
        let candidates = AuxWindowClassifier.classify(
            WindowServerWindows.onScreen(), sharedPIDs: sharedPIDs, inputMethodPIDs: imPIDs,
            trackedFrames: tracked, display: display)
        apply(candidates, shared: shared)
    }

    /// The diff half of `poll()`, separate so it is testable with no WindowServer.
    func apply(_ candidates: [AuxCandidate], shared: [WindowRegistry.Entry]) {
        let present = Set(candidates.map(\.windowID))

        let gone: [(CGWindowID, UInt64)] = lock.withLock {
            let missing = known.filter { !present.contains($0.key) }
            for key in missing.keys { known[key] = nil }
            return missing.map { ($0.key, $0.value.id) }
        }
        var menuClosed = false
        for (windowID, id) in gone {
            let wasMenu = registry.entry(for: id)?.kind == .menu
            if registry.removeAux(windowID: windowID) != nil {
                emit(.destroyed(id: id))
                menuClosed = menuClosed || wasMenu
            }
        }
        if menuClosed, !registry.auxSnapshot().contains(where: { $0.kind == .menu }) {
            onMenusClosed?()
        }

        // Oldest first, so a submenu finds the menu that opened it.
        for candidate in candidates.reversed() {
            let existing = lock.withLock { known[candidate.windowID] }
            if let existing {
                guard existing.rect != candidate.rect else { continue }
                lock.withLock { known[candidate.windowID] = (existing.id, candidate.rect) }
                if let entry = registry.entry(for: existing.id) {
                    registry.recordAux(
                        id: existing.id, rect: candidate.rect, kind: entry.kind,
                        parent: entry.parent, menuIndex: entry.menuIndex, ownerPID: candidate.pid)
                }
                emit(.moved(id: existing.id, rect: candidate.rect))
                continue
            }
            let (id, _) = registry.auxID(for: candidate.windowID)
            let openMenus = registry.auxSnapshot().filter {
                $0.kind == .menu && $0.ownerPID == candidate.pid
            }
            let resolved = AuxParentResolver.resolve(
                candidate: candidate, shared: shared, openMenus: openMenus,
                press: recentPress?(candidate.pid), focused: focusedWindow?())
            registry.recordAux(
                id: id, rect: candidate.rect, kind: candidate.kind, parent: resolved.parent,
                menuIndex: resolved.menuIndex, ownerPID: candidate.pid)
            lock.withLock { known[candidate.windowID] = (id, candidate.rect) }
            if resolved.menuIndex != nil { consumePress?(candidate.pid) }
            emit(
                .auxCreated(
                    id: id, rect: candidate.rect, kind: candidate.kind, parent: resolved.parent,
                    menuIndex: resolved.menuIndex))
        }
    }

    /// Forget everything (session stop). Ids already handed out are not reused.
    public func reset() {
        let windowIDs = lock.withLock { () -> [CGWindowID] in
            defer { known = [:] }
            return Array(known.keys)
        }
        for windowID in windowIDs { _ = registry.removeAux(windowID: windowID) }
    }
}
