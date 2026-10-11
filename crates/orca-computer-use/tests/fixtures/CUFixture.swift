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
edit.submenu = submenu; menu.addItem(edit); app.mainMenu = menu
let controller = Controller()
let window = ProbeWindow(contentRect: NSRect(x: 200, y: 200, width: 640, height: 480),
                      styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
window.title = "Orca CU Fixture"
let button = NSButton(title: "Increment", target: controller, action: #selector(Controller.increment(_:)))
button.frame = NSRect(x: 20, y: 420, width: 120, height: 32)
controller.label.frame = NSRect(x: 160, y: 426, width: 200, height: 20)
let field = NSTextField(frame: NSRect(x: 20, y: 380, width: 300, height: 24))
field.setAccessibilityLabel("Name"); field.delegate = controller
let secure = NSSecureTextField(frame: NSRect(x: 340, y: 380, width: 200, height: 24))
secure.setAccessibilityLabel("Password")
let scroll = NSScrollView(frame: NSRect(x: 20, y: 140, width: 300, height: 220))
let text = NSTextView(frame: scroll.bounds)
text.string = (1...200).map { "line \($0)" }.joined(separator: "\n")
scroll.documentView = text; scroll.hasVerticalScroller = true; scroll.setAccessibilityLabel("List")
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
window.makeKeyAndOrderFront(nil)
app.activate()
if foregroundPasswordProbe {
    DispatchQueue.main.async {
        window.makeFirstResponder(nil)
        log(["type": "focus_reset"])
    }
}
app.run()
