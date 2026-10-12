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
    override func isAccessibilityElement() -> Bool { false }
    override func accessibilityChildren() -> [Any]? { [] }
    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        log(["type": "canvas_click", "x": point.x, "y": point.y, "clicks": event.clickCount])
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
final class ProbeWindow: NSWindow {
    override func accessibilityPerformRaise() -> Bool {
        pendingProbeRaise = false
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
        if CommandLine.arguments.contains("--hide-focused-element") { return nil }
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
let button = ProbeButton(title: "Increment", target: controller, action: #selector(Controller.increment(_:)))
button.frame = NSRect(x: 20, y: 420, width: 120, height: 32)
controller.label.frame = NSRect(x: 160, y: 426, width: 200, height: 20)
final class ProbeField: NSTextField {
    override func accessibilitySubrole() -> NSAccessibility.Subrole? {
        // This probe must isolate the actual recipient check, not unknown-target gating.
        if CommandLine.arguments.contains("--sticky-password-focus") { return NSAccessibility.Subrole(rawValue: "AXUnknown") }
        if let value = CommandLine.arguments.first(where: { $0.hasPrefix("--subrole-error=") }), let code = Int(value.dropFirst("--subrole-error=".count)) { failAX(code); return nil }
        return super.accessibilitySubrole()
    }
    override func setAccessibilityFocused(_ focused: Bool) {
        if CommandLine.arguments.contains("--sticky-password-focus") || (foregroundTypeProbe && pendingProbeRaise) { return }
        super.setAccessibilityFocused(focused)
    }
    override func accessibilityValue() -> String? {
        if CommandLine.arguments.contains("--no-name-value") { return nil }
        return super.accessibilityValue()
    }
    override func accessibilityNumberOfCharacters() -> Int {
        return stringValue.utf16.count
    }
}
let field = ProbeField(frame: NSRect(x: 20, y: 380, width: 300, height: 24))
field.setAccessibilityLabel("Name"); field.delegate = controller
if CommandLine.arguments.contains("--prefilled-name") { field.stringValue = "a🦀c" }
if foregroundTypeProbe { field.font = NSFont.monospacedSystemFont(ofSize: 16, weight: .regular) }
let secure = NSSecureTextField(frame: NSRect(x: 340, y: 380, width: 200, height: 24))
secure.setAccessibilityLabel("Password")
let scroll = NSScrollView(frame: NSRect(x: 20, y: 140, width: 300, height: 220))
let text = NSTextView(frame: scroll.bounds)
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
for view in [button, controller.label, field, secure, scroll, canvas] as [NSView] {
    window.contentView!.addSubview(view)
}
if foregroundPasswordProbe || rejectEarlyHit {
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
