import AppKit

class AppDelegate: NSObject, NSApplicationDelegate {
    var statusItem: NSStatusItem!
    var statusMenuItem: NSMenuItem?
    var daemonProcess: Process?
    var uiProcess: Process?
    var uiReadiness: UIChildReadiness?
    var sigTermSource: DispatchSourceSignal?
    var childPids: [pid_t] = []
    var isQuitting = false
    var restartCount = 0
    let maxRestarts = 3
    var port = 9377
    var databaseSelection: DatabaseSelection?

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

        databaseSelection = DatabaseSelector.select(
            environment: ProcessInfo.processInfo.environment, home: NSHomeDirectory())
        guard let selection = databaseSelection else {
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
            startWebUI(selection: selection, runningStatus: "Running (external daemon)")
        } else {
            startDaemon(selection: selection)
            waitForDaemonSocket { [weak self] in
                guard let self = self else { return }
                self.startWebUI(selection: selection)
                self.scheduleHealthyReset(for: self.daemonProcess)
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

    func startDaemon(selection: DatabaseSelection) {
        let binaryPath = Bundle.main.bundlePath + "/Contents/MacOS/nestweaver-cli"

        // Start the daemon as a launchd Aqua LaunchAgent (`daemon start`), NOT as an NSTask
        // child (`daemon run`). candle's Metal shader compilation needs MTLCompilerService,
        // an Aqua per-session XPC service. A launchd agent runs in the user's GUI session and
        // reaches it (device=Metal); an NSTask child of this menubar app does NOT and falls
        // back to CPU — so the daemon must be launchd-managed to get the GPU. launchd
        // (KeepAlive) also owns crash-restart, so the app just waits for the socket. The
        // daemon is a shared service (MCP/CLI/UI all use it) and persists across app quits;
        // the app re-attaches via the external-daemon path on next launch.
        DispatchQueue.global().async { [weak self] in
            let process = Process()
            process.executableURL = URL(fileURLWithPath: binaryPath)
            process.arguments = selection.daemonArguments
            var environment = ProcessInfo.processInfo.environment
            // One line per diagnostic, so the alert below shows the whole
            // remedy rather than a terminal-width wrap of it.
            environment["NESTWEAVER_DIAGNOSTIC_WIDTH"] = "1000"
            process.environment = environment
            let stderrPipe = Pipe()
            process.standardError = stderrPipe
            do {
                try process.run()
                let stderrData = stderrPipe.fileHandleForReading.readDataToEndOfFile()
                process.waitUntilExit()
                if process.terminationStatus != 0 {
                    let stderrText = String(data: stderrData, encoding: .utf8) ?? ""
                    // `daemon start` refuses, before any daemon exists, a
                    // database this version must not open (built by an older
                    // storage engine) or one stuck on a frozen checkpoint log.
                    // Neither clears by retrying, and neither is retried here:
                    // show the CLI's own diagnostic, which names the one
                    // command or file move that fixes it.
                    let terminal = stderrText.contains("nestweaver::db_rebuild_required")
                        || stderrText.contains("frozen write-ahead log of a checkpoint")
                    DispatchQueue.main.async {
                        if terminal {
                            self?.updateStatus(
                                stderrText.contains("nestweaver::db_rebuild_required")
                                    ? "Database must be rebuilt" : "Database needs recovery")
                            let alert = NSAlert()
                            alert.messageText = "NestWeaver cannot open this database"
                            alert.informativeText = stderrText
                            alert.runModal()
                        } else {
                            self?.updateStatus("Daemon failed to start (\(process.terminationStatus))")
                        }
                    }
                }
            } catch {
                DispatchQueue.main.async {
                    let alert = NSAlert()
                    alert.messageText = "Failed to Start Daemon"
                    alert.informativeText = error.localizedDescription
                    alert.runModal()
                    NSApp.terminate(nil)
                }
            }
        }
    }

    func startWebUI(selection: DatabaseSelection, runningStatus: String = "Running") {
        if let old = uiProcess, old.isRunning {
            kill(old.processIdentifier, SIGTERM)
        }
        let binaryPath = Bundle.main.bundlePath + "/Contents/MacOS/nestweaver-cli"
        let process = Process()
        process.executableURL = URL(fileURLWithPath: binaryPath)
        process.arguments = selection.uiArguments(port: port)
        process.environment = ProcessInfo.processInfo.environment
        let readiness = UIChildReadiness()
        uiReadiness = readiness
        uiProcess = process
        updateStatus("Starting Web UI…")
        do {
            try readiness.launch(process, probe: { UIChildReadiness.healthy(port: $0) },
                onReady: { [weak self, weak process] actualPort in
                    DispatchQueue.main.async {
                        guard let self = self, let process = process,
                              self.uiProcess === process, !self.isQuitting,
                              readiness.permitsReady(process)
                              else { return }
                        self.port = actualPort
                        self.openWebUI()
                        self.updateStatus(runningStatus)
                    }
                }, onFailure: { [weak self, weak process] diagnostic in
                    DispatchQueue.main.async {
                        guard let self = self, let process = process,
                              self.uiProcess === process, !self.isQuitting else { return }
                        self.updateStatus("Web UI failed")
                        FileHandle.standardError.write(Data((diagnostic + "\n").utf8))
                    }
                })
            childPids.append(process.processIdentifier)
        } catch {
            updateStatus("Web UI failed to start")
            FileHandle.standardError.write(Data((error.localizedDescription + "\n").utf8))
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
}

// nw-448. Handle the standard informational flags BEFORE any AppKit object is
// created, so a probe cannot start or attach to a UI server as a side effect.
//
// `/Applications/<App>.app/Contents/MacOS/<Name>` is the conventional path a
// script or installer probes, and the natural probe is `--version`. Previously
// every argv fell through to "go start the UI": the probe printed the UI banner
// and then BLOCKED indefinitely. A hang is a worse failure mode than a non-zero
// exit because nothing surfaces a cause -- in CI it reads as a stalled step, and
// locally it leaves a server the caller never asked for.
//
// Only these exact flags are intercepted. Anything unrecognised keeps today's
// behaviour, which also leaves LaunchServices' own `-psn_...` argument alone.
func bundleShortVersion() -> String? {
    Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String
}

let informationalArgs = Set(CommandLine.arguments.dropFirst())

if !informationalArgs.isDisjoint(with: ["--version", "-V"]) {
    guard let version = bundleShortVersion() else {
        FileHandle.standardError.write(
            Data("NestWeaver: bundle is missing CFBundleShortVersionString\n".utf8))
        exit(1)
    }
    print("NestWeaver \(version)")
    exit(0)
}

if !informationalArgs.isDisjoint(with: ["--help", "-h"]) {
    let version = bundleShortVersion() ?? "unknown"
    print("""
    NestWeaver \(version) -- macOS menu-bar app.

    This is the GUI wrapper. Launched with no arguments it starts the NestWeaver
    daemon and UI server and installs a menu-bar item.

      --version, -V    print the bundle version and exit
      --help, -h       print this message and exit

    For the command-line interface use the CLI inside this bundle:
      \(Bundle.main.bundlePath)/Contents/MacOS/nestweaver-cli --help
    """)
    exit(0)
}

let delegate = AppDelegate()
NSApplication.shared.delegate = delegate
NSApplication.shared.setActivationPolicy(.accessory)

NSApplication.shared.run()
