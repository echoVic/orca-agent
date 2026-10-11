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
    override func draw(_ dirtyRect: NSRect) { NSColor.systemTeal.setFill(); dirtyRect.fill() }
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
final class ProbeWindow: NSWindow {
    override func accessibilityPerformRaise() -> Bool {
        pendingProbeRaise = false
        return super.accessibilityPerformRaise()
    }
    override func accessibilityHitTest(_ point: NSPoint) -> Any? {
        if foregroundPasswordProbe && pendingProbeRaise { return self }
        return super.accessibilityHitTest(point)
    }

}

@objc(CUProbeApplication)
final class ProbeApplication: NSApplication {
    override func accessibilityMainWindow() -> Any? {
        if CommandLine.arguments.contains("--no-main-window") { return nil }
        return super.accessibilityMainWindow()
    }
    override func accessibilityApplicationFocusedUIElement() -> Any? {
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
let button = NSButton(title: "Increment", target: controller, action: #selector(Controller.increment(_:)))
button.frame = NSRect(x: 20, y: 420, width: 120, height: 32)
controller.label.frame = NSRect(x: 160, y: 426, width: 200, height: 20)
final class ProbeField: NSTextField {
    override func setAccessibilityFocused(_ focused: Bool) {
        if CommandLine.arguments.contains("--sticky-password-focus") { return }
        super.setAccessibilityFocused(focused)
    }
}
let field = ProbeField(frame: NSRect(x: 20, y: 380, width: 300, height: 24))
field.setAccessibilityLabel("Name"); field.delegate = controller
if CommandLine.arguments.contains("--prefilled-name") { field.stringValue = "a🦀c" }
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
NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
    log(["type": "keydown", "chars": event.charactersIgnoringModifiers ?? "",
         "cmd": event.modifierFlags.contains(.command)])
    return event
}
let canvas = Canvas(frame: NSRect(x: 340, y: 140, width: 280, height: 220))
for view in [button, controller.label, field, secure, scroll, canvas] as [NSView] {
    window.contentView!.addSubview(view)
}
if foregroundPasswordProbe {
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
