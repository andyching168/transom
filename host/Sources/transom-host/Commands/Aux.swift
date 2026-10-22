import ArgumentParser
import CoreGraphics
import ScreenCaptureKit
import CoreImage
import CoreVideo
import ImageIO
import UniformTypeIdentifiers
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

    @Option(
        name: .long,
        help: "With --press: save the capture's frame (PNG) taken while the menu is open to this path.")
    var dump: String?

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

        let latest = LatestFrame()
        var dumpCapture: DisplayCapture?
        if dump != nil {
            let c = DisplayCapture(display: info, fps: 30, applicationPIDs: [target.pid])
            c.onPixelBuffer = { buffer, _ in latest.set(buffer) }
            try await c.start()
            dumpCapture = c
        }
        var dumped = false
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
            if let dump, !dumped, pressed, Date().timeIntervalSince(start) > 3,
                seen.values.contains(where: { $0.kind == .menu })
            {
                dumped = true
                dumpCapture?.requestRefresh()
                try await Task.sleep(nanoseconds: 300_000_000)
                print(latest.save(to: dump))
                try await Self.filterExperiment(
                    pid: target.pid, display: info, menuIDs: seen.values.filter { $0.kind == .menu }.map(\.windowID),
                    path: dump)
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
        await dumpCapture?.stop()
    }

    /// Which SCK filter shapes capture a menu? Prints what SCShareableContent knows
    /// and writes one PNG per variant next to `path`.
    private static func filterExperiment(
        pid: pid_t, display: DisplayInfo, menuIDs: [CGWindowID], path: String
    ) async throws {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        guard let scDisplay = content.displays.first(where: { $0.displayID == display.id }) else { return }
        let mine = content.windows.filter { $0.owningApplication?.processID == pid }
        print("experiment: SCK lists \(mine.count) Word windows; layers \(mine.map(\.windowLayer).sorted())")
        let menus = content.windows.filter { menuIDs.contains($0.windowID) }
        print("experiment: menu window in SCK list: \(!menus.isEmpty)")
        let apps = content.applications.filter { $0.processID == pid }
        let others = content.applications.filter { $0.processID != pid }
        let variants: [(String, SCContentFilter)] = [
            ("windows", SCContentFilter(display: scDisplay, including: mine)),
            ("excluding", SCContentFilter(display: scDisplay, excludingApplications: others, exceptingWindows: [])),
            ("apps+menu", SCContentFilter(display: scDisplay, including: apps, exceptingWindows: [])),
        ]
        for (name, filter) in variants {
            let latest = LatestFrame()
            let config = SCStreamConfiguration()
            config.width = display.pixelWidth
            config.height = display.pixelHeight
            config.pixelFormat = kCVPixelFormatType_32BGRA
            let stream = SCStream(filter: filter, configuration: config, delegate: nil)
            let sink = FrameSink(latest)
            try stream.addStreamOutput(sink, type: .screen, sampleHandlerQueue: DispatchQueue(label: "exp"))
            try await stream.startCapture()
            try await Task.sleep(for: .seconds(1))
            try await stream.stopCapture()
            print("experiment[\(name)]: " + latest.save(to: path.replacingOccurrences(of: ".png", with: "-\(name).png")))
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

/// The newest frame the capture delivered, for `--dump`.
private final class LatestFrame: @unchecked Sendable {
    private let lock = NSLock()
    private var buffer: CVPixelBuffer?
    func set(_ b: CVPixelBuffer) { lock.withLock { buffer = b } }
    func save(to path: String) -> String {
        guard let b = lock.withLock({ buffer }) else { return "dump: no frame received" }
        let image = CIImage(cvPixelBuffer: b)
        guard let cg = CIContext().createCGImage(image, from: image.extent),
            let dest = CGImageDestinationCreateWithURL(
                URL(fileURLWithPath: path) as CFURL, UTType.png.identifier as CFString, 1, nil)
        else { return "dump: could not encode" }
        CGImageDestinationAddImage(dest, cg, nil)
        return CGImageDestinationFinalize(dest)
            ? "dump: wrote \(path) (\(cg.width)x\(cg.height))" : "dump: write failed"
    }
}

private final class FrameSink: NSObject, SCStreamOutput, @unchecked Sendable {
    let latest: LatestFrame
    init(_ l: LatestFrame) { latest = l }
    func stream(_ s: SCStream, didOutputSampleBuffer b: CMSampleBuffer, of t: SCStreamOutputType) {
        guard t == .screen, let px = CMSampleBufferGetImageBuffer(b) else { return }
        latest.set(px)
    }
}
