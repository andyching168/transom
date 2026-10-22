import CoreGraphics
import CoreImage
import CoreMedia
import CoreVideo
import Foundation
import ScreenCaptureKit

/// A single ScreenCaptureKit display stream, configured at the display's
/// **native pixel size** so that SCK space == the display's pixel space (I-1).
///
/// This is the shared capture primitive behind both the `capture`/`probe` CLI
/// commands and the app's live probe view. It never scales: the stream config
/// width/height are set to the display's exact pixel dimensions, and the command
/// layer verifies the delivered buffer matches (a mismatch means SCK is scaling
/// and I-1 is already violated).
public final class DisplayCapture: NSObject, SCStreamOutput, @unchecked Sendable {

    /// What SCK was actually configured and is delivering, for I-1 verification.
    public struct FrameStats: Sendable {
        public let configuredWidth: Int
        public let configuredHeight: Int
        public let deliveredWidth: Int
        public let deliveredHeight: Int
        public let pixelFormat: OSType
        public var pixelFormatString: String { fourCC(pixelFormat) }
        /// True iff the delivered buffer matches the display's native pixel size.
        public let matchesNativePixels: Bool
    }

    private let display: DisplayInfo
    private let fps: Int
    private let applicationPIDs: Set<pid_t>?
    private let showsCursor: Bool
    private let pixelFormat: OSType
    private let queue = DispatchQueue(label: "one.transom.host.capture")
    // Accessed only on the capture queue, including explicit refreshes.
    private var lastPixelPTS = CMTime.invalid
    private var lastEmission = DispatchTime.now().uptimeNanoseconds

    private let lock = NSLock()
    private var stream: SCStream?
    private var extraPIDs: Set<pid_t> = []
    private var filterUpdateRunning = false
    private var excludedPIDs: Set<pid_t> = []
    private var exclusionTask: Task<Void, Never>?
    private var latestPixelBuffer: CVPixelBuffer?
    private var _stats: FrameStats?
    private let ciContext = CIContext(options: [.cacheIntermediates: false])

    /// Optional per-frame hook, called on the capture queue with a fresh CGImage.
    /// Used by the app's live view. Leave nil for the CLI's poll-on-demand model.
    public var onFrame: (@Sendable (CGImage) -> Void)?

    /// Optional per-frame hook, called on the capture queue with the **raw**
    /// IOSurface-backed pixel buffer and its presentation timestamp — the
    /// zero-copy entry point for the encoder (issue #3 Phase 2). Unlike `onFrame`,
    /// this never touches `CGImage`, so nothing round-trips through the CPU (I-1
    /// pipeline note). The buffer belongs to SCK's pool; use it **synchronously**
    /// (encode it now). Do not retain it past the call or the pool may recycle it
    /// underneath you.
    public var onPixelBuffer: (@Sendable (CVPixelBuffer, CMTime) -> Void)?

    public init(
        display: DisplayInfo, fps: Int = 60, applicationPIDs: Set<pid_t>? = nil,
        pixelFormat: OSType = kCVPixelFormatType_32BGRA, showsCursor: Bool = true
    ) {
        self.display = display
        self.fps = fps
        self.applicationPIDs = applicationPIDs
        self.pixelFormat = pixelFormat
        self.showsCursor = showsCursor
        super.init()
    }

    /// Stats from the most recent delivered frame, if any.
    public var stats: FrameStats? {
        lock.withLock { _stats }
    }

    /// Start the stream. Throws if SCK cannot see the display or Screen Recording
    /// permission is absent.
    public func start() async throws {
        let content = try await SCShareableContent.excludingDesktopWindows(
            false, onScreenWindowsOnly: false)
        guard let scDisplay = content.displays.first(where: { $0.displayID == display.id })
        else {
            throw CaptureError.displayNotFound(display.id)
        }

        let filter: SCContentFilter
        if let applicationPIDs {
            let wanted = applicationPIDs.union(lock.withLock { extraPIDs })
            guard let built = Self.appFilter(content: content, display: scDisplay, wanted: wanted)
            else { throw CaptureError.noSelectedApplications }
            filter = built.filter
            lock.withLock { excludedPIDs = built.excluded }
        } else {
            // Whole-display capture is reserved for the diagnostic CLI/probe.
            filter = SCContentFilter(display: scDisplay, excludingWindows: [])
        }

        let config = SCStreamConfiguration()
        // The load-bearing lines for I-1: exact native pixels, no scaling.
        config.width = display.pixelWidth
        config.height = display.pixelHeight
        config.pixelFormat = pixelFormat
        config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        config.queueDepth = 5
        config.showsCursor = showsCursor
        config.scalesToFit = false
        // ScreenCaptureKit declares backgroundColor as unowned(unsafe). Keep the
        // CGColor alive through SCStream's configuration copy; releasing the
        // temporary immediately after assignment leaves a dangling pointer and
        // crashes in CGColorCreateCopy when the app starts capture in the
        // background.
        let backgroundColor = CGColor(gray: 0, alpha: 1)
        config.backgroundColor = backgroundColor

        let stream = withExtendedLifetime(backgroundColor) {
            SCStream(filter: filter, configuration: config, delegate: nil)
        }
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        try await stream.startCapture()
        lock.withLock { self.stream = stream }
        if applicationPIDs != nil { startExclusionRefresh() }
    }

    /// A filter that shows the wanted apps and nothing of any other app.
    ///
    /// It *excludes* every other application rather than *including* the wanted
    /// ones: an inclusion filter silently drops an app's own menu windows (layer
    /// 101), which a selected window needs (measured on the Mac with
    /// `transom-host aux --dump`: inclusion by application or by window omits an
    /// open menu, exclusion keeps it).
    static func appFilter(
        content: SCShareableContent, display: SCDisplay, wanted: Set<pid_t>
    ) -> (filter: SCContentFilter, excluded: Set<pid_t>)? {
        guard content.applications.contains(where: { wanted.contains($0.processID) }) else {
            return nil
        }
        let others = content.applications.filter { !wanted.contains($0.processID) }
        return (
            SCContentFilter(display: display, excludingApplications: others, exceptingWindows: []),
            Set(others.map(\.processID))
        )
    }

    /// An app started after the stream did is not in its exclusion list and would
    /// show up on the display. Re-read the running apps and update the filter when
    /// the set changed.
    private func startExclusionRefresh() {
        let task = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(1))
                guard let self, !Task.isCancelled else { return }
                await self.refreshExclusions()
            }
        }
        lock.withLock { exclusionTask = task }
    }

    private func refreshExclusions() async {
        guard let base = applicationPIDs else { return }
        guard let content = try? await SCShareableContent.excludingDesktopWindows(
                false, onScreenWindowsOnly: false),
            let scDisplay = content.displays.first(where: { $0.displayID == display.id })
        else { return }
        let (extra, stream, applied) = lock.withLock { (extraPIDs, self.stream, excludedPIDs) }
        guard let stream,
            let built = Self.appFilter(content: content, display: scDisplay, wanted: base.union(extra)),
            built.excluded != applied
        else { return }
        do {
            try await stream.updateContentFilter(built.filter)
            lock.withLock { excludedPIDs = built.excluded }
        } catch {
            Log.capture.error(
                "capture: updateContentFilter failed: \(error.localizedDescription, privacy: .public)")
        }
    }

    /// Also capture the windows of these processes (input methods, whose
    /// candidate lists are drawn by their own process and so are not part of the
    /// selected apps). Applies to a running stream without restarting it, and is a
    /// no-op for a whole-display capture, which already sees everything.
    public func setAdditionalApplicationPIDs(_ pids: Set<pid_t>) async {
        guard applicationPIDs != nil else { return }
        let shouldRun: Bool = lock.withLock {
            guard extraPIDs != pids else { return false }
            extraPIDs = pids
            // One updater at a time; it re-reads `extraPIDs` until it settles.
            if filterUpdateRunning { return false }
            filterUpdateRunning = true
            return true
        }
        guard shouldRun else { return }
        while true {
            let (target, stream) = lock.withLock { (extraPIDs, self.stream) }
            if let stream, let base = applicationPIDs,
                let content = try? await SCShareableContent.excludingDesktopWindows(
                    false, onScreenWindowsOnly: false),
                let scDisplay = content.displays.first(where: { $0.displayID == display.id })
            {
                if let built = Self.appFilter(
                    content: content, display: scDisplay, wanted: base.union(target))
                {
                    do {
                        try await stream.updateContentFilter(built.filter)
                        lock.withLock { excludedPIDs = built.excluded }
                        Log.capture.notice(
                            "capture: filter updated (\(target.count, privacy: .public) input method)")
                    } catch {
                        Log.capture.error(
                            "capture: updateContentFilter failed: \(error.localizedDescription, privacy: .public)"
                        )
                    }
                }
            }
            let done: Bool = lock.withLock {
                if extraPIDs == target { filterUpdateRunning = false; return true }
                return false
            }
            if done { return }
        }
    }

    public func stop() async {
        lock.withLock { exclusionTask?.cancel(); exclusionTask = nil }
        let s: SCStream? = lock.withLock {
            let current = stream
            stream = nil
            return current
        }
        if let s { try? await s.stopCapture() }
        // Drain an in-flight explicit refresh before the caller finishes the
        // encoder. Refreshes queued after this observe stream == nil.
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            queue.async { continuation.resume() }
        }
    }

    /// SCK emits no complete frames while a display is idle. Feed its retained
    /// native-size image through the encoder on connect, then once more to
    /// release a decoder's one-frame pipeline delay. This changes no Mac UI.
    public func requestRefresh() {
        queue.async { [weak self] in self?.refreshOnCaptureQueue() }
        queue.asyncAfter(deadline: .now() + 0.05) { [weak self] in
            self?.refreshOnCaptureQueue()
        }
    }

    /// SCK stops producing complete samples when the screen settles. A decoder
    /// can still hold the final frame, or be waiting for a recovery keyframe.
    /// Keep idle sessions advancing without increasing an active stream's FPS.
    public func requestIdleRefresh() {
        queue.async { [weak self] in
            guard let self,
                DispatchTime.now().uptimeNanoseconds - self.lastEmission >= 250_000_000
            else { return }
            self.refreshOnCaptureQueue()
        }
    }

    private func refreshOnCaptureQueue() {
        let buffer = lock.withLock { stream == nil ? nil : latestPixelBuffer }
        guard let buffer else { return }
        emitPixelBuffer(buffer, pts: CMClockGetTime(CMClockGetHostTimeClock()))
    }

    private func emitPixelBuffer(_ buffer: CVPixelBuffer, pts: CMTime) {
        guard let pixelHook = onPixelBuffer else { return }
        // A captured sample can have been queued before an explicit refresh.
        // Keep encoder timestamps monotonic without altering any pixels.
        let timestamp = lastPixelPTS.isValid && CMTimeCompare(pts, lastPixelPTS) <= 0
            ? CMTimeAdd(lastPixelPTS, CMTime(value: 1, timescale: 1_000_000)) : pts
        lastPixelPTS = timestamp
        lastEmission = DispatchTime.now().uptimeNanoseconds
        let signpostState = Log.signposter.beginInterval("capture")
        pixelHook(buffer, timestamp)
        Log.signposter.endInterval("capture", signpostState)
    }

    /// Latest frame as a CGImage at **native pixels**, converted on demand. Nil
    /// until the first complete frame arrives. Never resamples (I-1): callers that
    /// only need a small preview let the display layer scale it down for drawing.
    ///
    /// The IOSurface-backed buffer is grabbed under the lock and the (potentially
    /// expensive) `createCGImage` runs **outside** it — holding a strong ref keeps
    /// the buffer alive against pool recycling, exactly as the capture callback's
    /// own `onFrame` render does — so a 60fps capture callback is never blocked
    /// waiting on a preview conversion.
    public func latestImage() -> CGImage? {
        let (buffer, ctx): (CVPixelBuffer?, CIContext) = lock.withLock {
            (latestPixelBuffer, ciContext)
        }
        guard let buffer else { return nil }
        return ctx.createCGImage(
            CIImage(cvPixelBuffer: buffer),
            from: CGRect(
                x: 0, y: 0,
                width: CVPixelBufferGetWidth(buffer),
                height: CVPixelBufferGetHeight(buffer)))
    }

    // MARK: - SCStreamOutput

    public func stream(
        _ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
        of type: SCStreamOutputType
    ) {
        guard type == .screen, sampleBuffer.isValid else { return }
        // Only act on complete frames; SCK also emits idle/blank status frames.
        guard
            let attachments = CMSampleBufferGetSampleAttachmentsArray(
                sampleBuffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
            let statusRaw = attachments.first?[.status] as? Int,
            let status = SCFrameStatus(rawValue: statusRaw),
            status == .complete
        else { return }

        guard let pixelBuffer = sampleBuffer.imageBuffer else { return }

        let dw = CVPixelBufferGetWidth(pixelBuffer)
        let dh = CVPixelBufferGetHeight(pixelBuffer)
        let fmt = CVPixelBufferGetPixelFormatType(pixelBuffer)
        let stats = FrameStats(
            configuredWidth: display.pixelWidth,
            configuredHeight: display.pixelHeight,
            deliveredWidth: dw,
            deliveredHeight: dh,
            pixelFormat: fmt,
            matchesNativePixels: dw == display.pixelWidth && dh == display.pixelHeight)

        let (ctx, frameHook):
            (
                CIContext, (@Sendable (CGImage) -> Void)?
            ) = lock.withLock {
                latestPixelBuffer = pixelBuffer
                _stats = stats
                return (ciContext, onFrame)
            }

        // Zero-copy tap first: hand the raw IOSurface buffer straight to the
        // encoder before spending anything on the CGImage path (I-1). Signposted
        // so the capture→handoff interval is measurable in Instruments.
        emitPixelBuffer(pixelBuffer, pts: sampleBuffer.presentationTimeStamp)

        if let frameHook {
            let image = ctx.createCGImage(
                CIImage(cvPixelBuffer: pixelBuffer),
                from: CGRect(x: 0, y: 0, width: dw, height: dh))
            if let image { frameHook(image) }
        }
    }
}

public enum CaptureError: Error, CustomStringConvertible {
    case displayNotFound(CGDirectDisplayID)
    case noSelectedApplications

    public var description: String {
        switch self {
        case .displayNotFound(let id):
            return "ScreenCaptureKit does not see display id \(id)."
        case .noSelectedApplications:
            return "ScreenCaptureKit cannot find the selected apps. Open an app window and try again."
        }
    }
}

/// Decode a four-character-code OSType (e.g. a CoreVideo pixel format) into its
/// printable form like `BGRA`.
public func fourCC(_ code: OSType) -> String {
    let bytes = [
        UInt8((code >> 24) & 0xFF),
        UInt8((code >> 16) & 0xFF),
        UInt8((code >> 8) & 0xFF),
        UInt8(code & 0xFF),
    ]
    let scalars = bytes.map { Character(UnicodeScalar($0)) }
    let s = String(scalars).trimmingCharacters(in: .whitespaces)
    return s.isEmpty ? String(code) : "\(s) (0x\(String(code, radix: 16)))"
}
