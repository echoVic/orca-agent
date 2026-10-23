// No window titles in state metadata. AX access is explicit, app+window scoped.
import AppKit
import ApplicationServices
import PDFKit

@_silgen_name("_AXUIElementGetWindow")
func axWindow(_ element: AXUIElement, _ id: UnsafeMutablePointer<CGWindowID>) -> AXError

func emit(_ value: Any) {
    let data = try! JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    print(String(data: data, encoding: .utf8)!)
}
func get(_ e: AXUIElement, _ key: String) -> CFTypeRef? {
    var value: CFTypeRef?
    _ = AXUIElementCopyAttributeValue(e, key as CFString, &value)
    return value
}
func children(_ e: AXUIElement, _ key: String = "AXChildren") -> [AXUIElement] {
    get(e, key) as? [AXUIElement] ?? []
}
func string(_ e: AXUIElement, _ key: String) -> String { get(e, key) as? String ?? "" }
func state() -> [String: Any] {
    let ws = NSWorkspace.shared
    let point = CGEvent(source: nil)!.location
    let windows = (CGWindowListCopyWindowInfo([.excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []).map { w in
        ["id": w[kCGWindowNumber as String] ?? 0, "pid": w[kCGWindowOwnerPID as String] ?? 0,
         "layer": w[kCGWindowLayer as String] ?? 0, "on_screen": w[kCGWindowIsOnscreen as String] ?? false,
         "bounds": w[kCGWindowBounds as String] ?? [:]] as [String: Any]
    }
    // NSScreen uses bottom-left coordinates; CGDisplayBounds uses CG window coordinates.
    var ids = [CGDirectDisplayID](repeating: 0, count: 32); var count: UInt32 = 0
    _ = CGGetActiveDisplayList(32, &ids, &count)
    let displays = ids.prefix(Int(count)).map { id -> [String: Double] in
        let r = CGDisplayBounds(id)
        return ["X": r.origin.x, "Y": r.origin.y, "Width": r.width, "Height": r.height]
    }
    return ["frontmost": ws.frontmostApplication?.bundleIdentifier ?? "", "front_pid": ws.frontmostApplication?.processIdentifier ?? 0,
            "pointer": [point.x, point.y], "windows": windows, "displays": displays,
            "apps": ws.runningApplications.filter { $0.activationPolicy == .regular }.map {
                ["bundle": $0.bundleIdentifier ?? "", "pid": $0.processIdentifier] as [String: Any]
            }, "ax_trusted": AXIsProcessTrusted(), "screen_capture": CGPreflightScreenCaptureAccess()]
}
var budget = 0
func walk(_ element: AXUIElement, _ depth: Int, _ buttons: Bool) -> [[String: Any]] {
    budget += 1
    if depth > 30 || budget > 4000 { return [] }
    let role = string(element, "AXRole")
    var result: [[String: Any]] = []
    if !buttons || role == "AXButton" {
        var item: [String: Any] = ["role": role]
        for key in ["AXTitle", "AXDescription", "AXValue", "AXIdentifier"] {
            if let value = get(element, key) as? String, !value.isEmpty { item[key] = value }
            else if let value = get(element, key) as? NSNumber { item[key] = value.stringValue }
        }
        if let selected = get(element, "AXSelected") as? Bool { item["AXSelected"] = selected }
        result.append(item)
    }
    for c in children(element) { result += walk(c, depth + 1, buttons) }
    return result
}
func discard(_ e: AXUIElement, _ depth: Int = 0) {
    if depth > 12 { return }
    if string(e, "AXRole") == "AXButton" && string(e, "AXIdentifier") == "DontSaveButton" {
        _ = AXUIElementPerformAction(e, "AXPress" as CFString)
    }
    for c in children(e) { discard(c, depth + 1) }
}
func windowID(_ e: AXUIElement) -> CGWindowID {
    var id: CGWindowID = 0; _ = axWindow(e, &id); return id
}
func find(_ e: AXUIElement, _ names: Set<String>, _ depth: Int = 0) -> [AXUIElement] {
    if depth > 12 { return [] }
    let named = names.contains(string(e, "AXDescription")) || names.contains(string(e, "AXTitle"))
    return (string(e, "AXRole") == "AXButton" && named ? [e] : []) + children(e).flatMap { find($0, names, depth + 1) }
}
// NSWorkspace/NSRunningApplication state only refreshes while the run loop runs.
func pump(_ seconds: Double) { RunLoop.current.run(until: Date().addingTimeInterval(seconds)) }
func focusedPid() -> pid_t {
    guard let app = get(AXUIElementCreateSystemWide(), "AXFocusedApplication") else { return 0 }
    var pid: pid_t = 0
    _ = AXUIElementGetPid(app as! AXUIElement, &pid)
    return pid
}
func waitFocused(_ pid: pid_t, _ seconds: Double) -> Bool {
    let end = Date().addingTimeInterval(seconds)
    while focusedPid() != pid && Date() < end { pump(0.05) }
    return focusedPid() == pid
}
func run(_ tool: String, _ arguments: [String]) {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: tool); process.arguments = arguments
    try? process.run(); process.waitUntilExit()
}
func key(_ code: CGKeyCode, _ flags: CGEventFlags = []) {
    for down in [true, false] {
        let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)!
        event.flags = flags; event.post(tap: .cghidEventTap)
    }
}
let args = CommandLine.arguments
let op = args[1]
if op == "state" {
    emit(state())
} else if op == "pdf-count" {
    if let doc = PDFDocument(url: URL(fileURLWithPath: args[2])) { emit(["pages": doc.pageCount]) }
    else { emit(["error": "PDFKit could not read fixture"]) }
} else if op == "restore" {
    let snapshot = try! JSONSerialization.jsonObject(with: Data(contentsOf: URL(fileURLWithPath: args[2]))) as! [String: Any]
    let pid = snapshot["front_pid"] as! Int32
    if let app = NSRunningApplication(processIdentifier: pid) {
        _ = app.activate(options: [.activateAllWindows])
    }
    if let p = snapshot["pointer"] as? [Double] { CGWarpMouseCursorPosition(CGPoint(x: p[0], y: p[1])) }
    Thread.sleep(forTimeInterval: 0.3)
    emit(state())
} else if op == "calculator-reset" {
    // Calculator restores its last display, so a stale result could answer the next task.
    let bundle = "com.apple.calculator"
    var app = NSRunningApplication.runningApplications(withBundleIdentifier: bundle).first
    let launched = app == nil
    if launched {
        run("/usr/bin/open", ["-g", "-b", bundle])
        let end = Date().addingTimeInterval(8)
        while app == nil && Date() < end { pump(0.1); app = NSRunningApplication.runningApplications(withBundleIdentifier: bundle).first }
    }
    guard let calculator = app else { emit(["error": "Calculator did not launch"]); exit(1) }
    let root = AXUIElementCreateApplication(calculator.processIdentifier)
    AXUIElementSetMessagingTimeout(root, 1)
    var pressed: [String] = []
    let end = Date().addingTimeInterval(8)
    while Date() < end {
        let windows = children(root, "AXWindows")
        if let all = windows.flatMap({ find($0, ["全部清除", "All Clear"]) }).first {
            _ = AXUIElementPerformAction(all, "AXPress" as CFString); pressed.append("all clear"); break
        }
        // C clears the entry first and then turns into AC.
        if pressed.count < 2, let clear = windows.flatMap({ find($0, ["清除", "Clear"]) }).first {
            _ = AXUIElementPerformAction(clear, "AXPress" as CFString); pressed.append("clear")
        }
        pump(0.1)
    }
    pump(0.2)
    let display = children(root, "AXWindows").flatMap { walk($0, 0, false) }
        .filter { $0["role"] as? String == "AXStaticText" }.compactMap { $0["AXValue"] as? String }
    if launched {
        _ = calculator.terminate()
        let quitEnd = Date().addingTimeInterval(8)
        while !calculator.isTerminated && Date() < quitEnd { pump(0.1) }
    }
    emit(["launched": launched, "pressed": pressed, "display": display, "terminated": calculator.isTerminated])
} else if let app = NSRunningApplication.runningApplications(withBundleIdentifier: args[2]).first {
    let root = AXUIElementCreateApplication(app.processIdentifier)
    AXUIElementSetMessagingTimeout(root, 1)
    if op == "quit" {
        if app.processIdentifier == Int32(args[3]) { emit(["terminated": app.terminate(), "pid": app.processIdentifier]) }
        else { emit(["error": "PID changed before cleanup"]) }
    } else if op == "new-browser" {
        let old = Set(children(root, "AXWindows").map { windowID($0) })
        _ = app.activate(options: [.activateAllWindows])
        if !waitFocused(app.processIdentifier, 1.5) {
            // LaunchServices activation still works when a background helper's request is ignored.
            run("/usr/bin/open", ["-b", args[2]])
        }
        guard waitFocused(app.processIdentifier, 3) else {
            emit(["error": "Browser activation did not complete", "focused_pid": focusedPid()]); exit(1)
        }
        pump(0.5)
        var added: [AXUIElement] = []
        for _ in 0..<2 {
            // A reopen event from `open` may already have created the window.
            if children(root, "AXWindows").allSatisfy({ old.contains(windowID($0)) }) {
                key(45, [.maskCommand]) // Cmd+N; prove creation before navigating.
            }
            let end = Date().addingTimeInterval(3)
            repeat {
                added = children(root, "AXWindows").filter { !old.contains(windowID($0)) }
                if added.count == 1 { break }
                pump(0.05)
            } while Date() < end
            if !added.isEmpty { break }
        }
        guard added.count == 1 else {
            emit(["error": "Could not identify a single new browser window", "new_windows": added.count]); exit(1)
        }
        let id = windowID(added[0])
        var focusedID: CGWindowID = 0
        let focusEnd = Date().addingTimeInterval(2)
        repeat {
            if let focused = get(root, "AXFocusedWindow") { focusedID = windowID(focused as! AXUIElement) }
            if focusedID == id && focusedPid() == app.processIdentifier { break }
            pump(0.05)
        } while Date() < focusEnd
        guard focusedID == id, focusedPid() == app.processIdentifier else {
            emit(["error": "New browser window is not focused", "focused_window": focusedID, "focused_pid": focusedPid()]); exit(1)
        }
        key(37, [.maskCommand]) // Cmd+L, confined to the newly proven window.
        let chars = Array("about:blank".utf16)
        for down in [true, false] {
            let event = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: down)!
            event.keyboardSetUnicodeString(stringLength: chars.count, unicodeString: chars)
            event.post(tap: .cghidEventTap)
        }
        key(36)
        Thread.sleep(forTimeInterval: 0.4)
        emit(["window_id": id, "blank_navigation_requested": true])
    } else {
        let id = UInt32(args[3])!
        let windows = children(root, "AXWindows").filter {
            var current: CGWindowID = 0; _ = axWindow($0, &current); return current == id
        }
        if windows.count != 1 { emit(["error": "Expected exactly one matching AX window"]) }
        else if op == "close" {
            if let button = get(windows[0], "AXCloseButton") {
                let code = AXUIElementPerformAction(button as! AXUIElement, "AXPress" as CFString)
                Thread.sleep(forTimeInterval: 0.2)
                if args[4] == "discard" { discard(windows[0]) }
                emit(["window": id, "close_error": code.rawValue])
            } else { emit(["error": "AXCloseButton absent"]) }
        } else {
            let nodes = walk(windows[0], 0, op == "buttons")
            if op == "settings" {
                // Persist only the task's appearance/chip fields, not account/sidebar data.
                let labels = ["浅色", "深色", "自动", "Light", "Dark", "Auto", "芯片", "Chip", "Apple M"]
                emit(nodes.filter { n in
                    ["AXTitle", "AXDescription", "AXValue"].contains { k in
                        guard let text = n[k] as? String else { return false }
                        return labels.contains { text.contains($0) }
                    }
                })
            } else { emit(nodes) }
        }
    }
} else { emit(["error": "Task application is not running"]) }
