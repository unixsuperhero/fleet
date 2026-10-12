import AppKit
import ApplicationServices

let args = CommandLine.arguments
func fail(_ message: String) -> Never { fputs(message + "\n", stderr); exit(1) }
guard args.count >= 3, let pid = Int32(args[1]), let app = NSRunningApplication(processIdentifier: pid) else { fail("Expected an owned live application PID and action") }
let action = args[2], wanted = args.count > 3 ? args[3] : ""
let root = AXUIElementCreateApplication(pid)
AXUIElementSetMessagingTimeout(root, 3)
func value(_ node: AXUIElement, _ key: String) -> CFTypeRef? {
    var result: CFTypeRef?
    return AXUIElementCopyAttributeValue(node, key as CFString, &result) == .success ? result : nil
}
func text(_ node: AXUIElement, _ key: String) -> String { value(node, key) as? String ?? "" }
func children(_ node: AXUIElement, _ key: String = kAXChildrenAttribute) -> [AXUIElement] { value(node, key) as? [AXUIElement] ?? [] }
let skipped: Set<String> = ["AXOutline", "AXTable", "AXScrollArea", "AXList", "AXBrowser", "AXRow", "AXColumn", "AXCell", "AXMenu", "AXWebArea"]
func nodes(_ node: AXUIElement, _ depth: Int = 0) -> [AXUIElement] {
    guard depth < 7 else { return [] }
    return children(node).flatMap { child in
        let role = text(child, kAXRoleAttribute)
        return [child] + (skipped.contains(role) ? [] : nodes(child, depth + 1))
    }
}
func activate() {
    app.activate(options: [])
    let until = Date().addingTimeInterval(3)
    while NSWorkspace.shared.frontmostApplication?.processIdentifier != pid && Date() < until {
        RunLoop.current.run(until: Date().addingTimeInterval(0.05))
    }
    guard NSWorkspace.shared.frontmostApplication?.processIdentifier == pid else { fail("Owned app \(pid) is not frontmost; observed \(NSWorkspace.shared.frontmostApplication?.processIdentifier ?? -1)") }
}
func key(_ code: CGKeyCode, _ flags: CGEventFlags = []) {
    guard NSWorkspace.shared.frontmostApplication?.processIdentifier == pid else { fail("Focus changed; refusing keyboard input") }
    for down in [true, false] { let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)!; event.flags = flags; event.post(tap: .cghidEventTap) }
}
if action == "front" { activate(); print("frontmost=true pid=\(pid)"); exit(0) }
let sheets = children(root, kAXWindowsAttribute).flatMap { children($0).filter { text($0, kAXRoleAttribute) == "AXSheet" } }
guard let sheet = sheets.first else { exit(0) }
if action == "info" {
    for node in nodes(sheet) {
        switch text(node, kAXRoleAttribute) {
        case "AXButton": print("button:" + text(node, kAXTitleAttribute))
        case "AXStaticText": print("text:" + text(node, kAXValueAttribute))
        default: break
        }
    }
} else if action == "click" {
    guard let button = nodes(sheet).first(where: { text($0, kAXRoleAttribute) == "AXButton" && text($0, kAXTitleAttribute) == wanted && (value($0, kAXEnabledAttribute) as? Bool == true) }) else { fail("Button missing or disabled: \(wanted)") }
    guard AXUIElementPerformAction(button, kAXPressAction as CFString) == .success else { fail("Native button press failed") }
    print("clicked")
} else if action == "goto" {
    activate(); key(5, [.maskCommand, .maskShift])
    Thread.sleep(forTimeInterval: 0.6)
    guard let focused = value(root, kAXFocusedUIElementAttribute), CFGetTypeID(focused) == AXUIElementGetTypeID() else { fail("Go-to-folder field is not focused") }
    let field = unsafeBitCast(focused, to: AXUIElement.self)
    guard ["AXTextField", "AXComboBox"].contains(text(field, kAXRoleAttribute)) else { fail("Refusing to type outside a native path field") }
    key(0, [.maskCommand])
    let characters = Array(wanted.utf16)
    for down in [true, false] {
        guard NSWorkspace.shared.frontmostApplication?.processIdentifier == pid else { fail("Focus changed; refusing path input") }
        let event = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: down)!
        event.keyboardSetUnicodeString(stringLength: characters.count, unicodeString: characters)
        event.post(tap: .cghidEventTap)
    }
    RunLoop.current.run(until: Date().addingTimeInterval(0.8))
    key(36); RunLoop.current.run(until: Date().addingTimeInterval(0.8)); print("typed")
} else { fail("Unknown action") }
