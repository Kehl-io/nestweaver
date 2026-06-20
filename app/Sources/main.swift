import AppKit

class AppDelegate: NSObject, NSApplicationDelegate {
    var statusItem: NSStatusItem!
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
                DispatchQueue.main.async { self.openWebUI() }
            }
        }
    }

    func setupMenuBar() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
        if let button = statusItem.button {
            if let iconURL = Bundle.main.url(forResource: "AppIcon", withExtension: "icns"),
               let icon = NSImage(contentsOf: iconURL) {
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
        let statusMenuItem = NSMenuItem(title: "Status: Running", action: nil, keyEquivalent: "")
        statusMenuItem.isEnabled = false
        menu.addItem(statusMenuItem)
        menu.addItem(NSMenuItem.separator())
        menu.addItem(NSMenuItem(title: "Quit NestWeaver", action: #selector(quitApp), keyEquivalent: "q"))
        statusItem.menu = menu
    }

    func startDaemon(dbPath: String) {
        let bundlePath = Bundle.main.bundlePath
        let binaryPath = bundlePath + "/Contents/MacOS/nestweaver"

        let process = Process()
        process.executableURL = URL(fileURLWithPath: binaryPath)
        process.arguments = ["daemon", "--db", dbPath, "run"]
        process.environment = ProcessInfo.processInfo.environment

        process.terminationHandler = { [weak self] proc in
            guard let self = self else { return }
            if proc.terminationReason == .uncaughtSignal && self.restartCount < self.maxRestarts {
                self.restartCount += 1
                DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
                    self.startDaemon(dbPath: dbPath)
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
        if let envDb = ProcessInfo.processInfo.environment["NESTWEAVER_DB"], !envDb.isEmpty {
            return envDb
        }

        let fm = FileManager.default
        let home = NSHomeDirectory()

        let instanceToml = home + "/.nestweaver/instance.toml"
        if let contents = try? String(contentsOfFile: instanceToml, encoding: .utf8) {
            for line in contents.components(separatedBy: "\n") {
                let trimmed = line.trimmingCharacters(in: .whitespaces)
                if trimmed.hasPrefix("db") && trimmed.contains("=") {
                    let value = trimmed.components(separatedBy: "=").last?
                        .trimmingCharacters(in: .whitespaces)
                        .trimmingCharacters(in: CharacterSet(charactersIn: "\"'"))
                    if let v = value, fm.fileExists(atPath: v) {
                        return v
                    }
                }
            }
        }

        let nestDir = home + "/.local/share/nestweaver"
        if let dirs = try? fm.contentsOfDirectory(atPath: nestDir) {
            for dir in dirs.sorted() {
                let dbPath = nestDir + "/" + dir + "/brain.lbug"
                if fm.fileExists(atPath: dbPath) {
                    return dbPath
                }
            }
        }

        return nil
    }
}

let delegate = AppDelegate()
NSApplication.shared.delegate = delegate
NSApplication.shared.setActivationPolicy(.accessory)
NSApplication.shared.run()
