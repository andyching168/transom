import Testing

@testable import TransomKit

/// The AX-role → pointer-shape table (`CursorGuess`). Pure; the AX read that feeds
/// it can only be exercised on the Mac.
@Suite("CursorGuess")
struct CursorGuessTests {

    private func shape(_ roles: String?...) -> CursorShape {
        CursorGuess.shape(chain: roles.map { CursorGuess.Link(role: $0) })
    }

    @Test("a text field is an I-beam, and so is a text area under a group")
    func text() {
        #expect(shape("AXTextField", "AXGroup", "AXWindow") == .ibeam)
        #expect(shape("AXTextArea", "AXScrollArea", "AXWindow") == .ibeam)
    }

    @Test("a link wins over the text inside it")
    func link() {
        #expect(shape("AXStaticText", "AXLink", "AXWebArea") == .pointingHand)
    }

    @Test("web text is an I-beam, but plain static text is not")
    func staticText() {
        #expect(shape("AXStaticText", "AXGroup", "AXWebArea", "AXWindow") == .ibeam)
        #expect(shape("AXStaticText", "AXGroup", "AXWindow") == .arrow)
        #expect(shape("AXGroup", "AXWebArea", "AXWindow") == .arrow)
    }

    @Test("a button inside web content keeps the arrow")
    func button() {
        #expect(shape("AXStaticText", "AXButton", "AXWebArea") == .arrow)
    }

    @Test("a splitter resizes across its orientation")
    func splitter() {
        let vertical = CursorGuess.Link(role: "AXSplitter", orientation: "AXVerticalOrientation")
        let horizontal = CursorGuess.Link(role: "AXSplitter", orientation: "AXHorizontalOrientation")
        #expect(CursorGuess.shape(chain: [vertical]) == .resizeLeftRight)
        #expect(CursorGuess.shape(chain: [horizontal]) == .resizeUpDown)
    }

    @Test("nothing recognised is the arrow")
    func fallback() {
        #expect(CursorGuess.shape(chain: []) == .arrow)
        #expect(shape(nil, "AXUnknown") == .arrow)
    }
}
