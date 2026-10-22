import ApplicationServices
import Foundation

// The Mac's pointer shape over a shared window, for a client that draws its own
// pointer.
//
// `InputDelivery.background` (the default) never moves the Mac's real cursor, so
// `NSCursor.currentSystem` says nothing about where the client's pointer is. What
// is under that point is still readable: the accessibility tree answers "which
// element is at this position" for an app in the background. The element's role
// (and its ancestors') decides the shape: a text field is an I-beam, a link is a
// pointing hand, a splitter is a resize cursor. That is a guess from roles, not
// the cursor the app would really set, so anything not recognised is the arrow.

/// Pure: AX roles → pointer shape.
public enum CursorGuess {

    /// One element on the path from the hit element up to its window.
    public struct Link: Equatable, Sendable {
        public var role: String?
        public var orientation: String?
        public init(role: String?, orientation: String? = nil) {
            self.role = role
            self.orientation = orientation
        }
    }

    /// Roles of controls that keep the arrow even when something above them (a
    /// link, a web area) would not.
    private static let arrowControls: Set<String> = [
        "AXButton", "AXCheckBox", "AXRadioButton", "AXPopUpButton", "AXMenuButton",
        "AXMenuItem", "AXMenuBarItem", "AXScrollBar", "AXSlider", "AXTabGroup",
        "AXToolbar", "AXValueIndicator", "AXIncrementor",
    ]

    /// `chain` is nearest first. The first decisive element wins.
    public static func shape(chain: [Link]) -> CursorShape {
        var sawStaticText = false
        for link in chain {
            switch link.role {
            case "AXLink": return .pointingHand
            case "AXTextField", "AXTextArea", "AXComboBox", "AXSearchField": return .ibeam
            case "AXSplitter":
                // A vertical splitter is a vertical bar between left and right panes.
                return link.orientation == "AXVerticalOrientation" ? .resizeLeftRight : .resizeUpDown
            case "AXStaticText": sawStaticText = true
            case "AXWebArea": return sawStaticText ? .ibeam : .arrow
            case "AXWindow", "AXSheet", "AXDrawer": return .arrow
            case .some(let role) where arrowControls.contains(role): return .arrow
            default: break
            }
        }
        return .arrow
    }
}

/// The AX half: read the elements at a point.
public enum CursorProbe {

    /// How far up from the hit element to look. Web content nests deeply, and a
    /// link's text is a few levels below the link.
    static let maxDepth = 8

    /// The shape for the point `point` (AX global points) in the app `pid`.
    public static func shape(pid: pid_t, at point: CGPoint) -> CursorShape {
        let app = AXUIElementCreateApplication(pid)
        // A beachballing app must not stall the probe for long.
        AXUIElementSetMessagingTimeout(app, 0.2)
        var hit: AXUIElement?
        guard
            AXUIElementCopyElementAtPosition(app, Float(point.x), Float(point.y), &hit) == .success,
            var element = hit
        else { return .arrow }
        var chain: [CursorGuess.Link] = []
        for _ in 0..<maxDepth {
            chain.append(
                CursorGuess.Link(
                    role: string(element, kAXRoleAttribute),
                    orientation: string(element, kAXOrientationAttribute)))
            var parent: CFTypeRef?
            guard
                AXUIElementCopyAttributeValue(element, kAXParentAttribute as CFString, &parent)
                    == .success,
                let parent, CFGetTypeID(parent) == AXUIElementGetTypeID()
            else { break }
            element = unsafeDowncast(parent, to: AXUIElement.self)
        }
        return CursorGuess.shape(chain: chain)
    }

    private static func string(_ element: AXUIElement, _ attribute: String) -> String? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, attribute as CFString, &value) == .success
        else { return nil }
        return value as? String
    }
}

/// Turns the client's pointer positions into `cursor` messages.
///
/// The AX read can take tens of milliseconds, so it runs on its own queue and only
/// the latest position is kept: a fast sweep probes a handful of points, not every
/// motion event, and input posting is never held up by it.
///
/// `@unchecked Sendable`: state is behind `lock`.
public final class CursorService: @unchecked Sendable {
    private struct Pending {
        var id: UInt64
        var point: CGPoint
    }

    private let registry: WindowRegistry
    private let emit: @Sendable (WindowWatcher.WindowEvent) -> Void
    private let queue = DispatchQueue(label: "transom.cursor", qos: .userInteractive)
    private let lock = NSLock()
    private var pending: Pending?
    private var draining = false
    private var last: [UInt64: CursorShape] = [:]

    /// Minimum time between two probes.
    static let probeInterval: TimeInterval = 0.04

    public init(
        registry: WindowRegistry, emit: @escaping @Sendable (WindowWatcher.WindowEvent) -> Void
    ) {
        self.registry = registry
        self.emit = emit
    }

    /// The client's pointer moved over window `id`; `point` is in AX global points.
    public func pointerMoved(id: UInt64, at point: CGPoint) {
        let start = lock.withLock { () -> Bool in
            pending = Pending(id: id, point: point)
            if draining { return false }
            draining = true
            return true
        }
        if start { queue.async { [self] in drain() } }
    }

    /// Forget what each window last showed, so the next client hears it again.
    public func reset() {
        lock.withLock { last = [:] }
    }

    private func drain() {
        while true {
            let job = lock.withLock { () -> Pending? in
                guard let job = pending else {
                    draining = false
                    return nil
                }
                pending = nil
                return job
            }
            guard let job else { return }
            if let pid = registry.entry(for: job.id)?.ownerPID {
                let shape = CursorProbe.shape(pid: pid, at: job.point)
                let changed = lock.withLock { () -> Bool in
                    guard last[job.id] != shape else { return false }
                    last[job.id] = shape
                    return true
                }
                if changed { emit(.cursor(id: job.id, shape: shape)) }
            }
            Thread.sleep(forTimeInterval: Self.probeInterval)
        }
    }
}
