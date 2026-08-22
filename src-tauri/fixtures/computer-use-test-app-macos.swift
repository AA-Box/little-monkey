import AppKit
import Foundation

final class TestAppDelegate: NSObject, NSApplicationDelegate {
    private var window: NSWindow!
    private var darkMode: NSButton!
    private var profileName: NSTextField!
    private var status: NSTextField!
    private var dynamicItems: NSStackView!
    private var dynamicCount = 0

    private var profileURL: URL {
        let temporaryDirectory = ProcessInfo.processInfo.environment["TMPDIR"] ?? "/tmp"
        return URL(fileURLWithPath: temporaryDirectory).appendingPathComponent("little-monkey-testapp-profile.json")
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let saved = loadProfile()
        let content = NSStackView()
        content.orientation = .vertical
        content.alignment = .leading
        content.spacing = 10
        content.edgeInsets = NSEdgeInsets(top: 18, left: 18, bottom: 18, right: 18)

        let heading = NSTextField(labelWithString: "Little Monkey TestApp")
        heading.font = NSFont.boldSystemFont(ofSize: 20)
        content.addArrangedSubview(heading)
        content.addArrangedSubview(NSTextField(labelWithString: "Native semantic-control acceptance fixture"))

        let profileRow = NSStackView()
        profileRow.orientation = .horizontal
        profileRow.spacing = 8
        let profileLabel = NSTextField(labelWithString: "Profile name")
        profileLabel.setContentHuggingPriority(.required, for: .horizontal)
        profileName = NSTextField(string: saved.profile)
        profileName.setAccessibilityLabel("Profile name")
        profileName.widthAnchor.constraint(equalToConstant: 250).isActive = true
        let save = NSButton(title: "Save profile", target: self, action: #selector(saveProfile))
        save.setAccessibilityLabel("Save profile")
        profileRow.addArrangedSubview(profileLabel)
        profileRow.addArrangedSubview(profileName)
        profileRow.addArrangedSubview(save)
        content.addArrangedSubview(profileRow)

        status = NSTextField(labelWithString: "Not saved")
        content.addArrangedSubview(status)

        darkMode = NSButton(checkboxWithTitle: "Dark mode", target: self, action: #selector(toggleDarkMode))
        darkMode.setAccessibilityLabel("Dark mode")
        darkMode.state = saved.dark ? .on : .off
        content.addArrangedSubview(darkMode)

        let openDialog = NSButton(title: "Open dialog", target: self, action: #selector(openDialog))
        openDialog.setAccessibilityLabel("Open dialog")
        content.addArrangedSubview(openDialog)
        let disabled = NSButton(title: "Disabled button", target: nil, action: nil)
        disabled.setAccessibilityLabel("Disabled button")
        disabled.isEnabled = false
        content.addArrangedSubview(disabled)

        let addDynamic = NSButton(title: "Add dynamic item", target: self, action: #selector(addDynamicItem))
        addDynamic.setAccessibilityLabel("Add dynamic item")
        content.addArrangedSubview(addDynamic)
        let destructive = NSButton(title: "Destructive action", target: self, action: #selector(destructiveAction))
        destructive.setAccessibilityLabel("Destructive action")
        content.addArrangedSubview(destructive)

        let password = NSSecureTextField(string: "")
        password.setAccessibilityLabel("Password")
        password.placeholderString = "Fake password field (must be blocked)"
        content.addArrangedSubview(password)

        dynamicItems = NSStackView()
        dynamicItems.orientation = .vertical
        dynamicItems.alignment = .leading
        content.addArrangedSubview(dynamicItems)

        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 640, height: 560),
            styleMask: [.titled, .closable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = "Little Monkey TestApp"
        window.contentView = content
        window.center()
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        applyTheme()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    @objc private func toggleDarkMode() {
        applyTheme()
        saveProfile()
    }

    @objc private func saveProfile() {
        let payload: [String: Any] = [
            "profile": profileName.stringValue,
            "dark": darkMode.state == .on,
        ]
        if JSONSerialization.isValidJSONObject(payload),
           let data = try? JSONSerialization.data(withJSONObject: payload) {
            try? data.write(to: profileURL, options: .atomic)
        }
        status.stringValue = "Saved"
    }

    @objc private func addDynamicItem() {
        dynamicCount += 1
        dynamicItems.addArrangedSubview(NSTextField(labelWithString: "Dynamic item \(dynamicCount)"))
    }

    @objc private func openDialog() {
        let alert = NSAlert()
        alert.messageText = "Test dialog"
        alert.informativeText = "Dialog opened successfully"
        alert.addButton(withTitle: "OK")
        alert.runModal()
    }

    @objc private func destructiveAction() {
        let alert = NSAlert()
        alert.messageText = "Confirm destructive action"
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Delete")
        if alert.runModal() == .alertSecondButtonReturn {
            try? FileManager.default.removeItem(at: profileURL)
            status.stringValue = "Profile deleted"
        }
    }

    private func applyTheme() {
        let isDark = darkMode.state == .on
        window?.backgroundColor = isDark ? NSColor(calibratedWhite: 0.12, alpha: 1) : NSColor.windowBackgroundColor
        status?.stringValue = isDark ? "Dark mode enabled" : "Light mode enabled"
    }

    private func loadProfile() -> (profile: String, dark: Bool) {
        guard let data = try? Data(contentsOf: profileURL),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            return ("Test profile", false)
        }
        let profile = object["profile"] as? String ?? "Test profile"
        let dark = object["dark"] as? Bool ?? false
        return (profile, dark)
    }
}

let app = NSApplication.shared
let delegate = TestAppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
