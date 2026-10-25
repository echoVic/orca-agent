import AppKit

let logPath = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "/tmp/cu-fixture.log"
func log(_ event: [String: Any]) {
    var line = event
    line["t"] = Date().timeIntervalSince1970
    guard let data = try? JSONSerialization.data(withJSONObject: line) else { return }
    let text = (String(data: data, encoding: .utf8) ?? "") + "\n"
    if let handle = FileHandle(forWritingAtPath: logPath) {
        handle.seekToEndOfFile(); handle.write(Data(text.utf8)); handle.closeFile()
    } else {
        FileManager.default.createFile(atPath: logPath, contents: Data(text.utf8))
    }
}

log(["type": "started", "pid": Int(ProcessInfo.processInfo.processIdentifier)])

final class Canvas: NSView {
    override var acceptsFirstResponder: Bool { true }
    override func isAccessibilityElement() -> Bool { false }
    override func isAccessibilityFocused() -> Bool { false }
    override func accessibilityChildren() -> [Any]? { [] }
    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        let point = convert(event.locationInWindow, from: nil)
        log(["type": "canvas_click", "x": point.x, "y": point.y, "clicks": event.clickCount])
    }
    override func keyDown(with event: NSEvent) {
        log(["type": "canvas_key", "characters": event.characters ?? ""])
    }
    override func rightMouseDown(with event: NSEvent) {
        log(["type": "canvas_right_click"])
    }
    override func draw(_ dirtyRect: NSRect) { NSColor.systemTeal.setFill(); dirtyRect.intersection(bounds).fill() }
}

final class Controller: NSObject, NSTextFieldDelegate {
    var count = 0
    let label = NSTextField(labelWithString: "Count: 0")
    @objc func increment(_ sender: Any?) {
        count += 1
        label.stringValue = "Count: \(count)"
        if CommandLine.arguments.contains("--rename-button-after-press"), let button = sender as? NSButton {
            button.title = "Renamed"
        }
        log(["type": "click", "count": count])
    }
    @objc func closeProbeWindow(_ sender: Any?) {
        window.close()
        log(["type": "closed"])
    }
    @objc func resetProbeFocus(_ sender: Any?) {
        pendingProbeRaise = true
        window.makeFirstResponder(nil)
        log(["type": "focus_reset"])
    }
    var newWindows: [NSWindow] = []
    @objc func newProbeWindow(_ sender: Any?) {
        let extra = NSWindow(contentRect: NSRect(x: 260, y: 160, width: 480, height: 320),
                             styleMask: [.titled, .closable], backing: .buffered, defer: false)
        extra.title = "Orca CU Fixture New"
        extra.isReleasedWhenClosed = false
        extra.makeKeyAndOrderFront(nil)
        newWindows.append(extra)
        log(["type": "new_window"])
    }

    func controlTextDidChange(_ note: Notification) {
        if let field = note.object as? NSTextField { log(["type": "input", "value": field.stringValue]) }
    }
}

// Opt-in regression state: background AX cannot focus the window, while
// foreground HID can focus the real secure field. Default fixture is unchanged.
var pendingProbeRaise = true
let foregroundPasswordProbe = CommandLine.arguments.contains("--foreground-password-probe")
let foregroundTypeProbe = CommandLine.arguments.contains("--foreground-type-probe")
let rejectEarlyHit = CommandLine.arguments.contains("--reject-hit-before-raise")
// Keep app-level focus absent through the foreground click decision, then
// reveal it for the unchanged wait_for_focus and system-recipient checks.
var raisedForFocusProbe = false
var omittedRaisedFocus = false
final class ProbeWindow: NSWindow {
    override func isAccessibilityModal() -> Bool {
        if let flag = CommandLine.arguments.first(where: { $0.hasPrefix("--window-modal-error=") }), let code = Int(flag.dropFirst("--window-modal-error=".count)) { failAX(code); return false }
        if CommandLine.arguments.contains("--modal-window") { return true }
        return super.isAccessibilityModal()
    }
    override func accessibilityPerformRaise() -> Bool {
        pendingProbeRaise = false
        if CommandLine.arguments.contains("--reveal-focus-after-raise-check") {
            raisedForFocusProbe = true
            omittedRaisedFocus = false
        }
        return super.accessibilityPerformRaise()
    }
    override func accessibilityHitTest(_ point: NSPoint) -> Any? {
        if rejectEarlyHit && pendingProbeRaise {
            log(["type": "early_hit"])
            return nil
        }
        if (foregroundPasswordProbe || foregroundTypeProbe) && pendingProbeRaise { return self }
        return super.accessibilityHitTest(point)
    }

}

// Native AX elements are weakly held by the server. Expire this focus handle
// between the length read and range write; the real Name field remains editable.
final class ExpiringSelectionFocus: NSAccessibilityElement {
    override func isAccessibilityElement() -> Bool { true }
    override func accessibilityIdentifier() -> String? { "expiring-name-focus" }
    override func accessibilityRole() -> NSAccessibility.Role? { .textField }
    override func accessibilityParent() -> Any? { field }
    override func accessibilityFrame() -> NSRect { field.accessibilityFrame() }
    override func accessibilityValue() -> Any? {
        if CommandLine.arguments.contains("--no-name-length") { return nil }
        let value = field.stringValue
        expiringSelectionFocus = nil
        log(["type": "selection_focus_expired"])
        return value
    }
    override func accessibilityNumberOfCharacters() -> Int {
        if CommandLine.arguments.contains("--no-name-length") {
            log(["type": "length_unavailable"])
            NSException(name: NSExceptionName("NSAccessibilityException"),
                        reason: "The fixture length is unavailable",
                        userInfo: [NSAccessibility.ErrorCodeExceptionInfo: NSNumber(value: -25212)]).raise()
            return 0
        }
        let length = field.stringValue.utf16.count
        expiringSelectionFocus = nil
        log(["type": "selection_focus_expired"])
        return length
    }
}
var expiringSelectionFocus: ExpiringSelectionFocus? = ExpiringSelectionFocus()

@objc(CUProbeApplication)
final class ProbeApplication: NSApplication {
    override func accessibilityHitTest(_ point: NSPoint) -> Any? {
        if rejectEarlyHit && pendingProbeRaise {
            log(["type": "early_hit"])
            return nil
        }
        return super.accessibilityHitTest(point)
    }
    override func accessibilityMainWindow() -> Any? {
        if CommandLine.arguments.contains("--no-main-window") { return nil }
        return super.accessibilityMainWindow()
    }
    override func accessibilityApplicationFocusedUIElement() -> Any? {
        if windows.contains(where: { $0.firstResponder is Canvas }) { return nil }
        if CommandLine.arguments.contains("--hide-focused-element") {
            if !raisedForFocusProbe || !omittedRaisedFocus {
                if raisedForFocusProbe {
                    omittedRaisedFocus = true
                    log(["type": "omitted_raised_app_focus"])
                }
                return nil
            }
        }
        if foregroundTypeProbe && pendingProbeRaise { return nil }
        if (CommandLine.arguments.contains("--expire-selection-focus") || CommandLine.arguments.contains("--no-name-length")), let focus = expiringSelectionFocus { return focus }
        if foregroundPasswordProbe, let window = windows.first(where: { $0.title == "Orca CU Fixture" }), (window.firstResponder == nil || window.firstResponder === window) { return nil }
        return super.accessibilityApplicationFocusedUIElement()
    }
}
let app = ProbeApplication.shared
app.setActivationPolicy(.regular)
// Standard AppKit shortcut dispatch needs an Edit menu, just like a real app.
let menu = NSMenu()
let edit = NSMenuItem(title: "Edit", action: nil, keyEquivalent: "")
let submenu = NSMenu(title: "Edit")
submenu.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
submenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
edit.submenu = submenu; menu.addItem(edit); app.mainMenu = menu
let controller = Controller()
// Cmd+N opens another window, as File > New Window does in a real app.
submenu.addItem(withTitle: "New Window", action: #selector(Controller.newProbeWindow(_:)),
                keyEquivalent: "n").target = controller

let window = ProbeWindow(contentRect: NSRect(x: 200, y: 200, width: 640, height: 480),
                      styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
window.title = "Orca CU Fixture"
func failAX(_ code: Int) {
    NSException(name: NSExceptionName("NSAccessibilityException"), reason: "Intentional fixture attribute failure",
                userInfo: [NSAccessibility.ErrorCodeExceptionInfo: NSNumber(value: code)]).raise()
}
final class ProbeButton: NSButton {
    override func accessibilityRole() -> NSAccessibility.Role? {
        if CommandLine.arguments.contains("--bad-button-role") { failAX(-25200); return nil }
        return super.accessibilityRole()
    }
    override func accessibilityFrame() -> NSRect {
        if CommandLine.arguments.contains("--bad-button-frame") { failAX(-25200); return .zero }
        return super.accessibilityFrame()
    }
    override func accessibilityChildren() -> [Any]? {
        if CommandLine.arguments.contains("--bad-button-children") { failAX(-25200); return nil }
        return super.accessibilityChildren()
    }
}
let button: NSButton = CommandLine.arguments.contains(where: { $0.hasPrefix("--bad-button-") })
    ? ProbeButton(title: "Increment", target: controller, action: #selector(Controller.increment(_:)))
    : NSButton(title: "Increment", target: controller, action: #selector(Controller.increment(_:)))
button.frame = NSRect(x: 20, y: 420, width: 120, height: 32)
controller.label.frame = NSRect(x: 160, y: 426, width: 200, height: 20)
final class ModalAncestor: NSAccessibilityElement {
    let index: Int
    var successor: Any?
    init(_ index: Int) { self.index = index; super.init() }
    override func accessibilityRole() -> NSAccessibility.Role? {
        if CommandLine.arguments.contains("--slow-modal-walk") {
            // 34 reads consume 13.6s, leaving room for foreground activation
            // before the shared 30s deadline without fitting 20s of typing.
            Thread.sleep(forTimeInterval: 0.4)
            log(["type": "slow_modal_read", "index": index])
            if index == 33 { return .sheet }
        }
        return .group
    }
    override func accessibilityParent() -> Any? { successor }
    override func accessibilityFrame() -> NSRect { field.accessibilityFrame() }
}
var modalAncestors: [ModalAncestor] = []

final class ProbeField: NSTextField {
    override func isAccessibilityFocused() -> Bool {
        if foregroundTypeProbe && pendingProbeRaise { return false }
        return window?.firstResponder === self || (currentEditor() != nil && window?.firstResponder === currentEditor())
    }
    override func accessibilityRole() -> NSAccessibility.Role? {
        if CommandLine.arguments.contains("--name-role-failure") { failAX(-25200); return nil }
        return .textField
    }
    override func accessibilityTopLevelUIElement() -> Any? {
        if let flag = CommandLine.arguments.first(where: { $0.hasPrefix("--modal-error=") }), let code = Int(flag.dropFirst("--modal-error=".count)) { failAX(code); return nil }
        if !modalAncestors.isEmpty { failAX(-25212); return nil }
        if CommandLine.arguments.contains("--modal-window") || CommandLine.arguments.contains("--name-role-failure") || CommandLine.arguments.contains(where: { $0.hasPrefix("--window-modal-error=") }) { return window }
        return window
    }
    override func accessibilityWindow() -> Any? {
        if !modalAncestors.isEmpty || CommandLine.arguments.contains("--modal-window") || CommandLine.arguments.contains(where: { $0.hasPrefix("--window-modal-error=") }) { return window }
        return window
    }
    override func accessibilityParent() -> Any? {
        if let first = modalAncestors.first { return first }
        return superview
    }

    override func accessibilitySubrole() -> NSAccessibility.Subrole? {
        // This probe must isolate the actual recipient check, not unknown-target gating.
        if CommandLine.arguments.contains("--sticky-password-focus") { return NSAccessibility.Subrole(rawValue: "AXUnknown") }
        if let value = CommandLine.arguments.first(where: { $0.hasPrefix("--subrole-error=") }), let code = Int(value.dropFirst("--subrole-error=".count)) { failAX(code); return nil }
        return NSAccessibility.Subrole(rawValue: "AXUnknown")
    }
    override func setAccessibilityFocused(_ focused: Bool) {
        if CommandLine.arguments.contains("--sticky-password-focus") || (foregroundTypeProbe && pendingProbeRaise) { return }
        super.setAccessibilityFocused(focused)
        if focused && CommandLine.arguments.contains("--select-name-on-focus"),
           let editor = currentEditor() as? NSTextView {
            editor.setSelectedRange(NSRange(location: 1, length: 2))
            let selection = editor.selectedRange()
            log(["type": "name_selection", "location": selection.location, "length": selection.length])
        }
    }
    override func accessibilityValue() -> String? {
        if CommandLine.arguments.contains("--no-name-value") { return nil }
        return super.accessibilityValue()
    }
    override func accessibilityNumberOfCharacters() -> Int {
        if CommandLine.arguments.contains("--select-name-on-focus") {
            log(["type": "name_length", "length": stringValue.utf16.count])
        }
        return stringValue.utf16.count
    }
}
let field: NSTextField = CommandLine.arguments.contains(where: { $0.hasPrefix("--") })
    ? ProbeField(frame: NSRect(x: 20, y: 380, width: 300, height: 24))
    : NSTextField(frame: NSRect(x: 20, y: 380, width: 300, height: 24))
field.setAccessibilityLabel("Name"); field.delegate = controller
if CommandLine.arguments.contains("--deep-modal-walk") || CommandLine.arguments.contains("--slow-modal-walk") || CommandLine.arguments.contains("--cyclic-modal-walk") {
    let count = CommandLine.arguments.contains("--slow-modal-walk") ? 34 : (CommandLine.arguments.contains("--cyclic-modal-walk") ? 1 : 40)
    modalAncestors = (0..<count).map { ModalAncestor($0) }
    for index in 0..<(count-1) { modalAncestors[index].successor = modalAncestors[index+1] }
    modalAncestors[count-1].successor = CommandLine.arguments.contains("--cyclic-modal-walk") ? modalAncestors[0] : window
}

if CommandLine.arguments.contains("--prefilled-name") { field.stringValue = "a🦀c" }
if foregroundTypeProbe { field.font = NSFont.monospacedSystemFont(ofSize: 16, weight: .regular) }
final class ProbeSecureField: NSSecureTextField {
    override func isAccessibilityFocused() -> Bool {
        if CommandLine.arguments.contains("--hide-focused-element") { return false }
        return super.isAccessibilityFocused()
    }
}
let secure: NSSecureTextField = CommandLine.arguments.contains("--hide-focused-element")
    ? ProbeSecureField(frame: NSRect(x: 340, y: 380, width: 200, height: 24))
    : NSSecureTextField(frame: NSRect(x: 340, y: 380, width: 200, height: 24))
secure.setAccessibilityLabel("Password")
let scroll = NSScrollView(frame: NSRect(x: 20, y: 140, width: 300, height: 220))
final class ProbeTextView: NSTextView {
    var becameSecure = false
    override func isAccessibilityFocused() -> Bool {
        if foregroundTypeProbe && pendingProbeRaise { return false }
        return window?.firstResponder === self
    }
    override func accessibilitySubrole() -> NSAccessibility.Subrole? {
        if becameSecure { return NSAccessibility.Subrole(rawValue: "AXSecureTextField") }
        return NSAccessibility.Subrole(rawValue: "AXUnknown")
    }
    private func afterControl() {
        if CommandLine.arguments.contains("--move-focus-after-control") {
            window?.makeFirstResponder(secure)
            log(["type": "control_focus_changed"])
        }
        if CommandLine.arguments.contains("--secure-after-control") {
            becameSecure = true
            log(["type": "control_recipient_secure"])
        }
    }
    override func insertTab(_ sender: Any?) { super.insertTab(sender); afterControl() }
    override func insertNewline(_ sender: Any?) { super.insertNewline(sender); afterControl() }
}
let text: NSTextView = foregroundTypeProbe || CommandLine.arguments.contains("--move-focus-after-control") || CommandLine.arguments.contains("--secure-after-control")
    ? ProbeTextView(frame: scroll.bounds) : NSTextView(frame: scroll.bounds)
text.string = (1...200).map { "line \($0)" }.joined(separator: "\n")
scroll.documentView = text; scroll.hasVerticalScroller = !CommandLine.arguments.contains("--foreground-scroll-probe");
if CommandLine.arguments.contains("--rows-probe") {
    let rows = NSView(frame: NSRect(x: 0, y: 0, width: 300, height: 3000))
    for index in 1...50 {
        let row = NSButton(title: "Row \(index)", target: controller, action: #selector(Controller.increment(_:)))
        row.setAccessibilityLabel("Row \(index)")
        row.frame = NSRect(x: 10, y: (index - 1) * 60, width: 250, height: 30)
        rows.addSubview(row)
    }
    scroll.documentView = rows
}
 scroll.setAccessibilityLabel("List")
scroll.contentView.postsBoundsChangedNotifications = true
NotificationCenter.default.addObserver(forName: NSView.boundsDidChangeNotification,
                                       object: scroll.contentView, queue: nil) { _ in
    log(["type": "scroll", "y": scroll.contentView.bounds.origin.y])
}
NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown) { event in
    if field.bounds.contains(field.convert(event.locationInWindow, from: nil)) {
        DispatchQueue.main.async {
            if let editor = field.currentEditor() as? NSTextView {
                if CommandLine.arguments.contains("--select-name-on-click") { editor.selectAll(nil) }
                log(["type": "name_click", "location": editor.selectedRange().location, "length": editor.selectedRange().length])
            }
        }
    }
    return event
}
NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
    log(["type": "keydown", "chars": event.charactersIgnoringModifiers ?? "",
         "cmd": event.modifierFlags.contains(.command)])
    return event
}
let canvas = Canvas(frame: NSRect(x: 340, y: 140, width: 280, height: 220))
let views: [NSView] = CommandLine.arguments.contains("--canvas-only")
    ? [canvas] : [button, controller.label, field, secure, scroll, canvas]
for view in views {
    window.contentView!.addSubview(view)
}
if foregroundPasswordProbe || foregroundTypeProbe || rejectEarlyHit {
    let reset = NSButton(title: "Reset probe focus", target: controller, action: #selector(Controller.resetProbeFocus(_:)))
    reset.frame = NSRect(x: 20, y: 20, width: 160, height: 30)
    window.contentView!.addSubview(reset)
}
let close = NSButton(title: "Close window", target: controller, action: #selector(Controller.closeProbeWindow(_:)))
close.frame = NSRect(x: 200, y: 20, width: 160, height: 30)
window.contentView!.addSubview(close)
window.makeKeyAndOrderFront(nil)
// Direct harness launches activate; NSWorkspace launch tests use its configuration.
if CommandLine.arguments.count > 1 { app.activate() }
if CommandLine.arguments.contains("--prefilled-name") {
    window.makeFirstResponder(field)
    if let editor = field.currentEditor() as? NSTextView {
        editor.setSelectedRange(NSRange(location: field.stringValue.utf16.count, length: 0))
    }
}
if foregroundPasswordProbe {
    DispatchQueue.main.async {
        window.makeFirstResponder(nil)
        log(["type": "focus_reset"])
    }
}
if CommandLine.arguments.contains("--sticky-password-focus") {
    window.makeFirstResponder(secure)
}
app.run()
