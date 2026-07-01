import AppKit

class AppDelegate: NSObject, NSApplicationDelegate {
    var statusItem: NSStatusItem!
    var statusMenuItem: NSMenuItem?
    var daemonProcess: Process?
    var uiProcess: Process?
    var sigTermSource: DispatchSourceSignal?
    var childPids: [pid_t] = []
    var isQuitting = false
    var restartCount = 0
    let maxRestarts = 3
    let port = 9377
    var dbPath: String?

    private static let socketDir = NSHomeDirectory() + "/.local/state/nestweaver"

    private func isDaemonSocketPresent() -> Bool {
        guard let dirs = try? FileManager.default.contentsOfDirectory(atPath: Self.socketDir) else { return false }
        return dirs.contains { FileManager.default.fileExists(atPath: Self.socketDir + "/" + $0 + "/daemon.sock") }
    }

    private func waitForDaemonSocket(timeout: Int = 100, then handler: @escaping () -> Void) {
        DispatchQueue.global().async { [weak self] in
            guard let self = self else { return }
            var found = false
            for _ in 0..<timeout {
                if self.isDaemonSocketPresent() { found = true; break }
                Thread.sleep(forTimeInterval: 0.1)
            }
            DispatchQueue.main.async {
                if found {
                    handler()
                } else {
                    self.updateStatus("Failed to start")
                }
            }
        }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        signal(SIGTERM, SIG_IGN)
        let source = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        source.setEventHandler { [weak self] in
            self?.quitApp()
        }
        source.resume()
        sigTermSource = source

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

        if isDaemonSocketPresent() {
            startWebUI(dbPath: db)
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
                self.openWebUI()
                self.updateStatus("Running (external daemon)")
            }
        } else {
            startDaemon(dbPath: db)
            waitForDaemonSocket { [weak self] in
                guard let self = self else { return }
                self.startWebUI(dbPath: db)
                self.scheduleHealthyReset(for: self.daemonProcess)
                DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
                    self.openWebUI()
                    self.updateStatus("Running")
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

    /// Reset the crash-restart counter only after the daemon has stayed up for a
    /// sustained window. A crashing daemon binds its UDS socket *before* it dies,
    /// so resetting the counter on mere socket appearance made `maxRestarts`
    /// unreachable and turned any crash into an endless ~1s respawn loop — one
    /// macOS "quit unexpectedly" notification per cycle. Guard on process identity
    /// so a daemon that crashes again before the window elapses does NOT reset.
    private func scheduleHealthyReset(for process: Process?) {
        let tracked = process
        DispatchQueue.main.asyncAfter(deadline: .now() + 60.0) { [weak self] in
            guard let self = self else { return }
            if self.daemonProcess === tracked, tracked?.isRunning == true {
                self.restartCount = 0
            }
        }
    }

    func startDaemon(dbPath: String) {
        let bundlePath = Bundle.main.bundlePath
        let binaryPath = bundlePath + "/Contents/MacOS/nestweaver-cli"

        let process = Process()
        process.executableURL = URL(fileURLWithPath: binaryPath)
        process.arguments = ["daemon", "--db", dbPath, "run"]
        process.environment = ProcessInfo.processInfo.environment

        process.terminationHandler = { [weak self] proc in
            DispatchQueue.main.async {
                guard let self = self, !self.isQuitting else { return }

                // Only check for external daemon on normal error exits —
                // signal-killed daemons leave stale sockets.
                if proc.terminationReason == .exit && proc.terminationStatus != 0 && self.isDaemonSocketPresent() {
                    self.daemonProcess = nil
                    self.updateStatus("Running (external daemon)")
                    self.startWebUI(dbPath: dbPath)
                } else if (proc.terminationReason == .uncaughtSignal || proc.terminationStatus != 0) && self.restartCount < self.maxRestarts {
                    self.restartCount += 1
                    self.updateStatus("Restarting (\(self.restartCount)/\(self.maxRestarts))…")
                    // Exponential backoff (1s, 2s, 4s, … capped at 30s) so a
                    // crash-looping daemon can't respawn every second.
                    let backoff = min(pow(2.0, Double(self.restartCount - 1)), 30.0)
                    DispatchQueue.main.asyncAfter(deadline: .now() + backoff) {
                        self.startDaemon(dbPath: dbPath)
                        self.waitForDaemonSocket { [weak self] in
                            guard let self = self else { return }
                            self.startWebUI(dbPath: dbPath)
                            // Reset the crash counter only after SUSTAINED uptime,
                            // never on mere socket appearance — see the helper.
                            self.scheduleHealthyReset(for: self.daemonProcess)
                            self.updateStatus("Running")
                        }
                    }
                } else if proc.terminationStatus != 0 && self.restartCount >= self.maxRestarts {
                    self.updateStatus("Stopped (too many crashes)")
                }
            }
        }

        do {
            try process.run()
            daemonProcess = process
            childPids.append(process.processIdentifier)
        } catch {
            let alert = NSAlert()
            alert.messageText = "Failed to Start Daemon"
            alert.informativeText = error.localizedDescription
            alert.runModal()
            NSApp.terminate(nil)
        }
    }

    func startWebUI(dbPath: String) {
        if let old = uiProcess, old.isRunning {
            kill(old.processIdentifier, SIGTERM)
        }
        let binaryPath = Bundle.main.bundlePath + "/Contents/MacOS/nestweaver-cli"
        let process = Process()
        process.executableURL = URL(fileURLWithPath: binaryPath)
        process.arguments = ["ui", "--port", String(port), "--no-open", "--db", dbPath]
        process.environment = ProcessInfo.processInfo.environment
        do {
            try process.run()
            uiProcess = process
            childPids.append(process.processIdentifier)
        } catch {
            updateStatus("Web UI failed to start")
        }
    }

    @objc func openWebUI() {
        if let url = URL(string: "http://127.0.0.1:\(port)") {
            NSWorkspace.shared.open(url)
        }
    }

    @objc func quitApp() {
        isQuitting = true
        terminateAllChildren()
        NSApp.terminate(nil)
    }

    func applicationWillTerminate(_ notification: Notification) {
        guard !isQuitting else { return }
        isQuitting = true
        terminateAllChildren()
    }

    private func terminateAllChildren() {
        let live = childPids.filter { kill($0, 0) == 0 }
        for pid in live { kill(pid, SIGTERM) }
        for _ in 0..<20 {
            if live.allSatisfy({ kill($0, 0) != 0 }) { return }
            usleep(50_000)
        }
        for pid in live { kill(pid, SIGKILL) }
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
