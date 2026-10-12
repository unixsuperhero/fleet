import AppKit
import ScreenCaptureKit

struct CaptureError: Error, CustomStringConvertible {
    let description: String
}

let arguments = Array(CommandLine.arguments.dropFirst())
let activateOnly = arguments.count == 3 && arguments[0] == "--pid" && arguments[2] == "--activate-only"
let capture = arguments.count == 4 && arguments[0] == "--pid" && arguments[2] == "--output" && arguments[3].hasPrefix("/")
guard activateOnly || capture, let pid = Int32(arguments[1]), pid > 0 else {
    fputs("Usage: xcrun swift scripts/native-capture.swift --pid PID (--activate-only | --output /absolute/image.png)\n", stderr)
    exit(2)
}

let application = NSApplication.shared
application.setActivationPolicy(.prohibited)

Task { @MainActor in
    do {
        let deadline = Date().addingTimeInterval(10)
        var ownedApp = NSRunningApplication(processIdentifier: pid)
        while (ownedApp == nil || ownedApp?.isFinishedLaunching == false) && Date() < deadline {
            if ownedApp?.isTerminated == true { break }
            try await Task.sleep(nanoseconds: 100_000_000)
            ownedApp = NSRunningApplication(processIdentifier: pid)
        }
        guard let ownedApp, !ownedApp.isTerminated, ownedApp.bundleIdentifier == "dev.hiiro.fleet" else {
            throw CaptureError(description: "PID \(pid) is not a running Fleet app.")
        }
        if activateOnly {
            guard ownedApp.isFinishedLaunching, ownedApp.activate(options: [.activateAllWindows]) else {
                throw CaptureError(description: "Could not activate owned Fleet PID \(pid).")
            }
            try await Task.sleep(nanoseconds: 1_000_000_000)
            print("Activated owned Fleet PID \(pid).")
        } else {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
            let windows = content.windows.filter {
                $0.owningApplication?.processID == pid &&
                $0.owningApplication?.bundleIdentifier == "dev.hiiro.fleet" &&
                $0.isOnScreen && $0.frame.width >= 880 && $0.frame.height >= 560
            }
            guard let window = windows.max(by: { $0.frame.width * $0.frame.height < $1.frame.width * $1.frame.height }) else {
                throw CaptureError(description: "No on-screen Fleet main window (at least 880x560) belongs to PID \(pid).")
            }
            let filter = SCContentFilter(desktopIndependentWindow: window)
            let configuration = SCStreamConfiguration()
            configuration.width = Int((filter.contentRect.width * CGFloat(filter.pointPixelScale)).rounded())
            configuration.height = Int((filter.contentRect.height * CGFloat(filter.pointPixelScale)).rounded())
            configuration.showsCursor = false
            configuration.ignoreShadowsSingleWindow = true
            let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: configuration)
            guard let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:]) else {
                throw CaptureError(description: "Could not encode owned Fleet window as PNG.")
            }
            try png.write(to: URL(fileURLWithPath: arguments[3]), options: .atomic)
            print("Captured Fleet PID \(pid), window \(window.windowID), \(image.width)x\(image.height): \(arguments[3])")
        }
        exit(0)
    } catch {
        fputs("Native capture failed: \(error)\n", stderr)
        exit(1)
    }
}
RunLoop.main.run()
