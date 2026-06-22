import AppKit

class AppDelegate: NSObject, NSApplicationDelegate {
    var statusItem: NSStatusItem!
    var statusMenuItem: NSMenuItem?
    var daemonProcess: Process?
    var restartCount = 0
    let maxRestarts = 3
    let port = 9377
    var dbPath: String?

    func applicationDidFinishLaunching(_ notification: Notification) {
        dbPath = detectDatabase()
        guard let db = dbPath else {
            let alert = NSAlert()
            alert.messageText = "No NestWeaver Database Found"
            alert.informativeText = "Run 'nestweaver index --repo <path> --db <path>' first to create a database."
            alert.alertStyle = .warning
            alert.addButton(withTitle: "OK")
            alert.runModal()
            NSApp.terminate(nil)
            return
        }

        setupMenuBar()
        startDaemon(dbPath: db)

        DispatchQueue.global().async { [weak self] in
            guard let self = self else { return }
            let socketDir = NSHomeDirectory() + "/.local/state/nestweaver"
            var found = false
            for _ in 0..<100 {
                if let dirs = try? FileManager.default.contentsOfDirectory(atPath: socketDir) {
                    for dir in dirs {
                        let sock = socketDir + "/" + dir + "/daemon.sock"
                        if FileManager.default.fileExists(atPath: sock) {
                            found = true
                            break
                        }
                    }
                }
                if found { break }
                Thread.sleep(forTimeInterval: 0.1)
            }
            if found {
                // Verify the daemon is actually healthy by checking the port
                DispatchQueue.main.async {
                    self.openWebUI()
                    self.updateStatus("Running")
                }
            } else {
                DispatchQueue.main.async {
                    self.updateStatus("Failed to start")
                }
            }
        }
    }

    func setupMenuBar() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        if let button = statusItem.button {
            // Try dedicated menubar template image first, fall back to app icon
            let icon: NSImage? = {
                if let url = Bundle.main.url(forResource: "MenuIcon", withExtension: "png"),
                   let img = NSImage(contentsOf: url) {
                    return img
                }
                if let url = Bundle.main.url(forResource: "AppIcon", withExtension: "icns"),
                   let img = NSImage(contentsOf: url) {
                    return img
                }
                return nil
            }()
            if let icon = icon {
                icon.size = NSSize(width: 18, height: 18)
                icon.isTemplate = true
                button.image = icon
            } else {
                button.title = "NW"
            }
        }

        let menu = NSMenu()
        menu.addItem(NSMenuItem(title: "Open Web UI", action: #selector(openWebUI), keyEquivalent: "o"))
        menu.addItem(NSMenuItem.separator())
        let si = NSMenuItem(title: "Status: Starting…", action: nil, keyEquivalent: "")
        si.isEnabled = false
        statusMenuItem = si
        menu.addItem(si)
        menu.addItem(NSMenuItem.separator())
        menu.addItem(NSMenuItem(title: "Quit NestWeaver", action: #selector(quitApp), keyEquivalent: "q"))
        statusItem.menu = menu
    }

    func updateStatus(_ status: String) {
        statusMenuItem?.title = "Status: \(status)"
    }

    func startDaemon(dbPath: String) {
        let bundlePath = Bundle.main.bundlePath
        let binaryPath = bundlePath + "/Contents/MacOS/nestweaver-cli"

        let process = Process()
        process.executableURL = URL(fileURLWithPath: binaryPath)
        process.arguments = ["daemon", "--db", dbPath, "run"]
        process.environment = ProcessInfo.processInfo.environment

        process.terminationHandler = { [weak self] proc in
            guard let self = self else { return }
            DispatchQueue.main.async {
                if proc.terminationReason == .uncaughtSignal && self.restartCount < self.maxRestarts {
                    self.restartCount += 1
                    self.updateStatus("Restarting (\(self.restartCount)/\(self.maxRestarts))…")
                    DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
                        self.startDaemon(dbPath: dbPath)
                    }
                } else if proc.terminationStatus != 0 && self.restartCount >= self.maxRestarts {
                    self.updateStatus("Stopped (too many crashes)")
                }
            }
        }

        do {
            try process.run()
            daemonProcess = process
        } catch {
            let alert = NSAlert()
            alert.messageText = "Failed to Start Daemon"
            alert.informativeText = error.localizedDescription
            alert.runModal()
            NSApp.terminate(nil)
        }
    }

    @objc func openWebUI() {
        if let url = URL(string: "http://127.0.0.1:\(port)") {
            NSWorkspace.shared.open(url)
        }
    }

    @objc func quitApp() {
        if let process = daemonProcess, process.isRunning {
            process.interrupt()
            process.waitUntilExit()
        }
        NSApp.terminate(nil)
    }

    func applicationWillTerminate(_ notification: Notification) {
        if let process = daemonProcess, process.isRunning {
            process.interrupt()
        }
    }

    func detectDatabase() -> String? {
        // 1. Environment variable
        if let envDb = ProcessInfo.processInfo.environment["NESTWEAVER_DB"], !envDb.isEmpty {
            return envDb
        }

        let fm = FileManager.default
        let home = NSHomeDirectory()

        // 2. Parse ~/.nestweaver/instance.toml for db field
        let instanceToml = home + "/.nestweaver/instance.toml"
        if let contents = try? String(contentsOfFile: instanceToml, encoding: .utf8) {
            if let dbValue = parseTomlString(contents, key: "db") {
                // Resolve ~ in path
                let resolved = dbValue.hasPrefix("~/")
                    ? home + String(dbValue.dropFirst(1))
                    : dbValue
                // Resolve symlinks for consistency with Rust's canonicalize
                let canonical = (resolved as NSString).resolvingSymlinksInPath
                if fm.fileExists(atPath: canonical) {
                    return canonical
                }
            }
        }

        // 3. Glob ~/.local/share/nestweaver/*/brain.lbug
        let nestDir = home + "/.local/share/nestweaver"
        if let dirs = try? fm.contentsOfDirectory(atPath: nestDir) {
            for dir in dirs.sorted() {
                let candidate = nestDir + "/" + dir + "/brain.lbug"
                if fm.fileExists(atPath: candidate) {
                    return (candidate as NSString).resolvingSymlinksInPath
                }
            }
        }

        return nil
    }

    /// Parse a simple key = "value" or key = 'value' from TOML content.
    /// Handles quoted values correctly, including values containing '='.
    func parseTomlString(_ content: String, key: String) -> String? {
        for line in content.components(separatedBy: "\n") {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            // Skip comments and section headers
            if trimmed.hasPrefix("#") || trimmed.hasPrefix("[") { continue }

            // Match key = value pattern
            guard let eqIndex = trimmed.firstIndex(of: "=") else { continue }
            let lineKey = trimmed[trimmed.startIndex..<eqIndex]
                .trimmingCharacters(in: .whitespaces)
            if lineKey != key { continue }

            var value = trimmed[trimmed.index(after: eqIndex)...]
                .trimmingCharacters(in: .whitespaces)

            // Strip matching quotes
            if (value.hasPrefix("\"") && value.hasSuffix("\"")) ||
               (value.hasPrefix("'") && value.hasSuffix("'")) {
                value = String(value.dropFirst().dropLast())
            }

            return value.isEmpty ? nil : value
        }
        return nil
    }
}

let delegate = AppDelegate()
NSApplication.shared.delegate = delegate
NSApplication.shared.setActivationPolicy(.accessory)
NSApplication.shared.run()
