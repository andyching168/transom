import ApplicationServices
import CoreGraphics
import Foundation
import Testing

@testable import TransomKit

private let display = DisplayInfo(
    id: 1, originPoints: .zero, sizePoints: CGSize(width: 1920, height: 1080),
    pixelWidth: 3840, pixelHeight: 2160, scale: 2, isMain: true)

private func window(
    _ id: CGWindowID, pid: pid_t, layer: Int, _ bounds: CGRect, alpha: Double = 1
) -> WindowServerWindow {
    WindowServerWindow(id: id, pid: pid, layer: layer, bounds: bounds, alpha: alpha)
}

private func classify(
    _ windows: [WindowServerWindow], shared: Set<pid_t> = [10], inputMethods: Set<pid_t> = [],
    tracked: [CGRect] = []
) -> [AuxCandidate] {
    AuxWindowClassifier.classify(
        windows, sharedPIDs: shared, inputMethodPIDs: inputMethods, trackedFrames: tracked,
        display: display)
}

@Suite("Transient window classification")
struct AuxClassifierTests {

    @Test("a menu of a shared app is a menu, in VDS pixels (points x scale)")
    func menuOfSharedApp() throws {
        // Measured on the Mac: a Finder menu is layer 101 at (102,25) 243x631 pt.
        let found = classify([window(1, pid: 10, layer: 101, CGRect(x: 102, y: 25, width: 243, height: 631))])
        let menu = try #require(found.first)
        #expect(menu.kind == .menu)
        #expect(menu.rect == WireRect(x: 204, y: 50, w: 486, h: 1262))
        #expect(!menu.isInputMethod)
    }

    @Test("a non-menu transient window of a shared app is a popover")
    func popover() {
        let found = classify([window(1, pid: 10, layer: 3, CGRect(x: 10, y: 10, width: 100, height: 100))])
        #expect(found.map(\.kind) == [.popover])
    }

    @Test("normal windows, other apps, the menu bar and status items are not transient")
    func exclusions() {
        let rect = CGRect(x: 10, y: 10, width: 100, height: 100)
        #expect(classify([window(1, pid: 10, layer: 0, rect)]).isEmpty)  // AX's job
        #expect(classify([window(2, pid: 99, layer: 101, rect)]).isEmpty)  // not shared
        #expect(classify([window(3, pid: 10, layer: 24, rect)]).isEmpty)  // Menubar
        #expect(classify([window(4, pid: 10, layer: 25, rect)]).isEmpty)  // status item
        #expect(classify([window(5, pid: 10, layer: 2_147_483_630, rect)]).isEmpty)  // cursor
        #expect(classify([window(6, pid: 10, layer: 101, rect, alpha: 0)]).isEmpty)
        #expect(classify([window(7, pid: 10, layer: 101, CGRect(x: 0, y: 0, width: 2, height: 2))]).isEmpty)
    }

    @Test("a floating panel AX already tracks is not reported twice")
    func trackedPanel() {
        let frame = CGRect(x: 50, y: 60, width: 300, height: 200)
        let found = classify(
            [window(1, pid: 10, layer: 3, frame.insetBy(dx: 0.5, dy: 0.5))], tracked: [frame])
        #expect(found.isEmpty)
    }

    @Test("an input method's candidate window is mirrored even though it is not a shared app")
    func inputMethodCandidates() throws {
        let found = classify(
            [window(1, pid: 77, layer: 101, CGRect(x: 400, y: 300, width: 220, height: 40))],
            inputMethods: [77])
        let candidate = try #require(found.first)
        #expect(candidate.isInputMethod)
        #expect(candidate.kind == .popover)
        #expect(candidate.rect == WireRect(x: 800, y: 600, w: 440, h: 80))
    }

    @Test("an input method's own normal window (preferences) is not transient")
    func inputMethodPreferences() {
        #expect(
            classify(
                [window(1, pid: 77, layer: 0, CGRect(x: 400, y: 300, width: 220, height: 40))],
                inputMethods: [77]
            ).isEmpty)
    }

    @Test("a popup hanging off the display is clipped, and its origin stays honest")
    func clipping() throws {
        // 100pt wide, 50pt of it past the right edge of the 1920pt display.
        let rect = try #require(
            AuxWindowClassifier.captureRect(
                CGRect(x: 1870, y: 100, width: 100, height: 40), display: display))
        #expect(rect == WireRect(x: 3740, y: 200, w: 100, h: 80))
        // Entirely off the display: nothing to show.
        #expect(
            AuxWindowClassifier.captureRect(
                CGRect(x: 2000, y: 100, width: 100, height: 40), display: display) == nil)
        // Off the top-left: clipped to 0, not wrapped to a huge u32.
        let topLeft = AuxWindowClassifier.captureRect(
            CGRect(x: -30, y: -10, width: 100, height: 40), display: display)
        #expect(topLeft == WireRect(x: 0, y: 0, w: 140, h: 60))
    }

    @Test("non-finite frames are refused")
    func nonFinite() {
        #expect(AuxWindowClassifier.captureRect(.null, display: display) == nil)
        #expect(AuxWindowClassifier.captureRect(.infinite, display: display) == nil)
    }

    @Test("front-to-back order and the per-app cap are kept")
    func cap() {
        let windows = (0..<40).map {
            window(CGWindowID($0 + 1), pid: 10, layer: 101,
                CGRect(x: 10 * $0, y: 10, width: 8, height: 8))
        }
        let found = classify(windows)
        #expect(found.count == AuxWindowClassifier.maximumCount)
        #expect(found.first?.windowID == 1)
    }
}

@Suite("Transient window parent")
struct AuxParentTests {
    private func shared(_ id: UInt64, pid: pid_t, _ x: UInt32, _ y: UInt32, _ w: UInt32 = 1000, _ h: UInt32 = 800)
        -> WindowRegistry.Entry
    {
        WindowRegistry.Entry(
            id: id, rect: WireRect(x: x, y: y, w: w, h: h), title: "", ownerPID: pid)
    }
    private func candidate(
        _ kind: WindowKind, pid: pid_t = 10, _ x: UInt32, _ y: UInt32, im: Bool = false
    ) -> AuxCandidate {
        AuxCandidate(
            windowID: 1, pid: pid, layer: 101, kind: kind, rect: WireRect(x: x, y: y, w: 200, h: 300),
            isInputMethod: im)
    }

    @Test("a menu opened from the client's strip is anchored to that item")
    func pressedMenu() {
        let result = AuxParentResolver.resolve(
            candidate: candidate(.menu, 50, 50), shared: [shared(1, pid: 10, 0, 100)],
            openMenus: [], press: .init(windowID: 1, index: 3), focused: nil)
        #expect(result == .init(parent: 1, menuIndex: 3))
    }

    @Test("a submenu hangs off the open menu, not off the strip")
    func submenu() {
        let open = WindowRegistry.Entry(
            id: 50, rect: WireRect(x: 0, y: 0, w: 1, h: 1), title: "", kind: .menu, parent: 1,
            menuIndex: 3, ownerPID: 10)
        let result = AuxParentResolver.resolve(
            candidate: candidate(.menu, 300, 80), shared: [shared(1, pid: 10, 0, 100)],
            openMenus: [open], press: .init(windowID: 1, index: 3), focused: nil)
        #expect(result == .init(parent: 50, menuIndex: nil))
    }

    @Test("a context menu with no press goes to the window it overlaps")
    func contextMenu() {
        let result = AuxParentResolver.resolve(
            candidate: candidate(.menu, 1500, 400),
            shared: [shared(1, pid: 10, 0, 100), shared(2, pid: 10, 1300, 100)],
            openMenus: [], press: nil, focused: 1)
        #expect(result == .init(parent: 2, menuIndex: nil))
    }

    @Test("with overlaps the own app's window wins over a bigger overlap by another app")
    func ownAppPreferred() {
        let result = AuxParentResolver.resolve(
            candidate: candidate(.popover, pid: 10, 900, 200),
            shared: [shared(1, pid: 20, 0, 100, 2000, 800), shared(2, pid: 10, 850, 150, 100, 400)],
            openMenus: [], press: nil, focused: nil)
        #expect(result.parent == 2)
    }

    @Test("a popup that overlaps nothing falls back to the focused window, then the first")
    func fallbacks() {
        let windows = [shared(1, pid: 10, 0, 0, 100, 100), shared(2, pid: 10, 500, 0, 100, 100)]
        let far = candidate(.popover, 3000, 1500)
        #expect(
            AuxParentResolver.resolve(candidate: far, shared: windows, openMenus: [], press: nil, focused: 2).parent == 2)
        #expect(
            AuxParentResolver.resolve(candidate: far, shared: windows, openMenus: [], press: nil, focused: 99).parent == 1)
        #expect(
            AuxParentResolver.resolve(candidate: far, shared: [], openMenus: [], press: nil, focused: nil).parent == nil)
    }

    @Test("an input method's candidate list sits on the shared window being typed into")
    func inputMethod() {
        let result = AuxParentResolver.resolve(
            candidate: candidate(.popover, pid: 77, 700, 400, im: true),
            shared: [shared(1, pid: 10, 0, 0, 500, 500), shared(2, pid: 10, 600, 300, 900, 700)],
            openMenus: [], press: nil, focused: 1)
        #expect(result.parent == 2)
    }
}

@Suite("Transient window tracking")
struct AuxTrackerTests {
    final class Events: @unchecked Sendable {
        private let lock = NSLock()
        private var all: [WindowWatcher.WindowEvent] = []
        func add(_ e: WindowWatcher.WindowEvent) { lock.withLock { all.append(e) } }
        var events: [WindowWatcher.WindowEvent] { lock.withLock { all } }
    }

    private func make() -> (AuxWindowTracker, WindowRegistry, Events, UInt64) {
        let registry = WindowRegistry()
        let events = Events()
        let tracker = AuxWindowTracker(
            display: display, registry: registry, sharedPIDs: [10], emit: { events.add($0) })
        let main = registry.nextSharedIDForTesting(pid: 10, rect: WireRect(x: 0, y: 100, w: 2000, h: 1500))
        return (tracker, registry, events, main)
    }

    private func menu(_ windowID: CGWindowID, y: UInt32 = 50, h: UInt32 = 600) -> AuxCandidate {
        AuxCandidate(
            windowID: windowID, pid: 10, layer: 101, kind: .menu,
            rect: WireRect(x: 200, y: y, w: 400, h: h), isInputMethod: false)
    }

    @Test("appear, move, disappear: one create, one move, one destroy, and the entry is gone")
    func lifecycle() throws {
        let (tracker, registry, events, main) = make()
        tracker.apply([menu(5)], shared: registry.snapshot())
        tracker.apply([menu(5)], shared: registry.snapshot())  // unchanged: nothing
        tracker.apply([menu(5, h: 700)], shared: registry.snapshot())
        tracker.apply([], shared: registry.snapshot())

        let all = events.events
        #expect(all.count == 3)
        guard case let .auxCreated(id, _, kind, parent, _) = all[0] else {
            Issue.record("first event should be auxCreated: \(all)")
            return
        }
        #expect(kind == .menu)
        #expect(parent == main)
        #expect(all[1] == .moved(id: id, rect: WireRect(x: 200, y: 50, w: 400, h: 700)))
        #expect(all[2] == .destroyed(id: id))
        #expect(registry.auxSnapshot().isEmpty)
        #expect(registry.entry(for: id) == nil)
    }

    @Test("transient windows never enter the shared snapshot the tiler and resize use")
    func notTiled() {
        let (tracker, registry, _, main) = make()
        tracker.apply([menu(5)], shared: registry.snapshot())
        #expect(registry.snapshot().map(\.id) == [main])
        #expect(registry.auxSnapshot().count == 1)
    }

    @Test("ids are not reused when the same WindowServer window reappears as a new one")
    func idsNotReused() throws {
        let (tracker, registry, events, _) = make()
        tracker.apply([menu(5)], shared: registry.snapshot())
        tracker.apply([], shared: registry.snapshot())
        tracker.apply([menu(6)], shared: registry.snapshot())
        let created = events.events.compactMap { event -> UInt64? in
            if case let .auxCreated(id, _, _, _, _) = event { return id }
            return nil
        }
        #expect(created.count == 2)
        #expect(created[0] != created[1])
    }

    @Test("a press that is fresh anchors the menu to its strip item")
    func pressAnchors() throws {
        let (tracker, registry, events, main) = make()
        tracker.recentPress = { _ in .init(windowID: main, index: 4) }
        tracker.apply([menu(5)], shared: registry.snapshot())
        guard case let .auxCreated(_, _, _, parent, menuIndex)? = events.events.first else {
            Issue.record("no auxCreated")
            return
        }
        #expect(parent == main)
        #expect(menuIndex == 4)
    }

    @Test("a resync lists transient windows after their parent, outside the tile layout")
    func resyncContents() {
        let (tracker, registry, _, main) = make()
        tracker.apply([menu(5)], shared: registry.snapshot())
        let aux = registry.auxSnapshot()
        #expect(aux.count == 1)
        #expect(aux[0].parent == main)
        #expect(aux[0].kind == .menu)
        #expect(aux[0].ownerPID == 10)
    }
}

extension WindowRegistry {
    /// Register a shared window without AX, for tests.
    func nextSharedIDForTesting(pid: pid_t, rect: WireRect) -> UInt64 {
        let (id, _) = auxID(for: 0xFFFF_0000 &+ UInt32(truncatingIfNeeded: pid))
        recordAux(id: id, rect: rect, kind: .normal, parent: nil, menuIndex: nil, ownerPID: pid)
        return id
    }
}

@Suite("Menu bar items")
struct MenuBarTests {
    @Test("the Apple menu and untitled items are dropped; indices stay the AX positions")
    func titles() {
        let items = MenuBar.titles(of: [
            ("Apple", true), ("Finder", true), ("檔案", true), (nil, true), ("", true),
            ("編輯", false),
        ])
        #expect(
            items == [
                WireMenuItem(index: 1, title: "Finder"),
                WireMenuItem(index: 2, title: "檔案"),
                WireMenuItem(index: 5, title: "編輯", enabled: false),
            ])
    }

    @Test("a press for an unknown window fails cleanly instead of touching AX")
    func unknownWindow() {
        let service = MenuBarService(registry: WindowRegistry(), emit: { _ in })
        guard case .failed = service.press(id: 12345, index: 1) else {
            Issue.record("expected failure")
            return
        }
    }

    @Test("a press that did not open a menu is forgotten, so it cannot anchor a later popup")
    func failedPressForgotten() {
        let registry = WindowRegistry()
        let id = registry.nextSharedIDForTesting(pid: 4_000_000, rect: WireRect(x: 0, y: 0, w: 10, h: 10))
        let service = MenuBarService(registry: registry, emit: { _ in })
        // The pid does not exist, so AX fails.
        guard case .failed = service.press(id: id, index: 2) else {
            Issue.record("expected failure")
            return
        }
        #expect(service.recentPress(pid: 4_000_000) == nil)
    }
}

@Suite("Input method detection")
struct InputMethodPathTests {
    @Test("third-party and system input method executables are recognised")
    func paths() {
        #expect(InputMethodProcesses.isInputMethod(
            executablePath: "/Users/a/Library/Input Methods/vChewing.app/Contents/MacOS/vChewing"))
        #expect(InputMethodProcesses.isInputMethod(
            executablePath: "/Library/Input Methods/SogouInput.app/Contents/SogouServices"))
        #expect(InputMethodProcesses.isInputMethod(
            executablePath: "/System/Library/PrivateFrameworks/TextInputUIMacHelper.framework/Versions/A/XPCServices/CursorUIViewService.xpc/Contents/MacOS/CursorUIViewService"))
    }

    @Test("ordinary apps and the menu extra for the input source are not")
    func notInputMethods() {
        #expect(!InputMethodProcesses.isInputMethod(executablePath: "/Applications/Microsoft Word.app/Contents/MacOS/Microsoft Word"))
        #expect(!InputMethodProcesses.isInputMethod(executablePath: "/System/Library/CoreServices/TextInputMenuAgent.app/Contents/MacOS/TextInputMenuAgent"))
    }
}
