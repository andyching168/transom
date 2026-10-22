import ArgumentParser
import CoreGraphics
import Foundation
import TransomKit

/// `aux <app>` — show what the host would mirror to the client as transient
/// windows (menus, popovers, input-method candidate lists), the app's menu bar,
/// and optionally press a menu-bar item to see its menu appear.
///
/// This is the diagnostic for the three things a plain window crop misses: it
/// prints the classification live, so a popup that does not reach the client can
/// be told apart from one the host never saw. Run it while opening a menu, a
/// drop-down, or typing with an input method.
struct Aux: AsyncParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "aux",
        abstract: "List transient windows (menus, popovers, IME) and the menu bar of an app.")

    @Argument(help: "Shared app: a name like \"Microsoft Word\" or a bundle id.")
    var app: String

    @Option(name: .long, help: "Display id (default: the main display).")
    var display: UInt32?

    @Option(name: .long, help: "Watch for this many seconds.")
    var seconds: Double = 10

    @Option(name: .long, help: "Press menu-bar item N a moment after starting, cancel it at the end.")
    var press: Int?

    @Flag(
        name: .long,
        help: "Also start a capture of the app and add the input-method processes to it live, reporting frames.")
    var captureCheck = false

    func run() async throws {
        let target: TargetApp
        switch AppResolver.resolve(app) {
        case .success(let t): target = t
        case .failure(let e): throw e
        }
        guard
            let info = display.flatMap({ TransomKit.Displays.byID($0) })
                ?? TransomKit.Displays.all().first(where: { $0.isMain })
        else { throw ProbeError("No display found.") }

        let items = MenuBar.items(pid: target.pid)
        print("\(target.name) (pid \(target.pid)) on display \(info.id): "
            + "\(info.pixelWidth)x\(info.pixelHeight) px @\(info.scale)x")
        if let items {
            print("menu bar: " + items.map { "[\($0.index)] \($0.title)\($0.enabled ? "" : " (disabled)")" }.joined(separator: "  "))
        } else {
            print("menu bar: unreadable (is Accessibility granted?)")
        }
        let ims = InputMethodProcesses.scan()
        print("input-method processes: \(ims.sorted())")

        if captureCheck { try await Self.checkCapture(target: target, display: info, inputMethods: ims) }

        var seen: [CGWindowID: AuxCandidate] = [:]
        let start = Date()
        var pressed = false
        var cancelled = false
        while Date().timeIntervalSince(start) < seconds {
            if let press, !pressed, Date().timeIntervalSince(start) > 1 {
                pressed = true
                print("press [\(press)] -> \(MenuBar.press(pid: target.pid, index: press))")
            }
            let found = AuxWindowClassifier.classify(
                WindowServerWindows.onScreen(), sharedPIDs: [target.pid], inputMethodPIDs: ims,
                trackedFrames: [], display: info)
            for candidate in found where seen[candidate.windowID] != candidate {
                let verb = seen[candidate.windowID] == nil ? "appeared" : "moved   "
                seen[candidate.windowID] = candidate
                let r = candidate.rect
                print(String(format: "%6.2fs %@ window %u pid %d layer %d kind %@%@ rect %d,%d %dx%d px",
                    Date().timeIntervalSince(start), verb, candidate.windowID, candidate.pid,
                    candidate.layer, candidate.kind.rawValue,
                    candidate.isInputMethod ? " (input method)" : "", r.x, r.y, r.w, r.h))
            }
            for gone in seen.keys where !found.contains(where: { $0.windowID == gone }) {
                print(String(format: "%6.2fs gone     window %u", Date().timeIntervalSince(start), gone))
                seen[gone] = nil
            }
            if let press, pressed, !cancelled, Date().timeIntervalSince(start) > seconds - 1 {
                cancelled = true
                print("press [\(press)] again -> \(MenuBar.press(pid: target.pid, index: press))")
            }
            try await Task.sleep(nanoseconds: 40_000_000)
        }
    }

    /// Prove the capture survives widening its filter to the input methods while it
    /// is running (the path a new candidate-list process takes mid-session).
    private static func checkCapture(target: TargetApp, display: DisplayInfo, inputMethods: Set<pid_t>)
        async throws
    {
        final class Counter: @unchecked Sendable {
            private let lock = NSLock()
            private var n = 0
            func bump() { lock.withLock { n += 1 } }
            var value: Int { lock.withLock { n } }
        }
        let counter = Counter()
        let capture = DisplayCapture(display: display, fps: 30, applicationPIDs: [target.pid])
        capture.onPixelBuffer = { _, _ in counter.bump() }
        try await capture.start()
        try await Task.sleep(for: .seconds(1))
        capture.requestRefresh()
        try await Task.sleep(for: .milliseconds(300))
        print("capture: \(counter.value) frames with only \(target.name); native pixels: \(capture.stats?.matchesNativePixels ?? false)")
        let before = counter.value
        await capture.setAdditionalApplicationPIDs(inputMethods)
        capture.requestRefresh()
        try await Task.sleep(for: .seconds(1))
        print("capture: +\(counter.value - before) frames after adding \(inputMethods.count) input-method pids; native pixels: \(capture.stats?.matchesNativePixels ?? false)")
        await capture.setAdditionalApplicationPIDs([])
        capture.requestRefresh()
        try await Task.sleep(for: .milliseconds(500))
        print("capture: \(counter.value) frames after removing them again")
        await capture.stop()
    }
}
