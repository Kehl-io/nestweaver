import Foundation

/// A nonce on this child's stdout proves successful bind/daemon attachment.
/// HTTP health alone could belong to an unrelated process occupying the port.
final class UIChildReadiness {
    private let lock = NSLock()
    private var stderr = Data()
    private var stdout = Data()
    private enum LaunchMode: String { case attached, supervised, direct }
    private var announcedMode: LaunchMode?
    private var announcedPort: Int?
    private var failed = false
    private var ready = false

    private func failure(_ message: String, _ callback: @escaping (String) -> Void) {
        lock.lock()
        guard !failed else { lock.unlock(); return }
        failed = true
        let detail = String(decoding: stderr, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        lock.unlock()
        callback(detail.isEmpty ? message : message + ": " + detail)
    }

    func launch(_ process: Process, timeout: TimeInterval = 15,
                probe: @escaping (Int) -> Bool, onReady: @escaping (Int) -> Void,
                onFailure: @escaping (String) -> Void) throws {
        let token = UUID().uuidString
        var environment = process.environment ?? ProcessInfo.processInfo.environment
        environment["NESTWEAVER_LAUNCHER_READY_TOKEN"] = token
        process.environment = environment
        let errorPipe = Pipe()
        let outputPipe = Pipe()
        process.standardError = errorPipe
        process.standardOutput = outputPipe
        let drained = DispatchGroup()
        for (pipe, isError) in [(errorPipe, true), (outputPipe, false)] {
            drained.enter()
            DispatchQueue.global().async {
                defer { drained.leave() }
                while true {
                    let data = pipe.fileHandleForReading.availableData
                    if data.isEmpty { break }
                    self.lock.lock()
                    if isError {
                        self.stderr.append(data.prefix(max(0, 8192 - self.stderr.count)))
                    } else {
                        self.stdout.append(data)
                        if self.stdout.count > 8192 { self.stdout.removeFirst(self.stdout.count - 8192) }
                        // A partial token/port is never readiness evidence.
                        for line in String(decoding: self.stdout, as: UTF8.self).components(separatedBy: "\n").dropLast() {
                            let prefix = "NW_UI_READY:" + token + ":"
                            if line.hasPrefix(prefix) {
                                let fields = line.dropFirst(prefix.count).split(separator: ":", omittingEmptySubsequences: false)
                                if fields.count == 2, let mode = LaunchMode(rawValue: String(fields[0])),
                                   let port = Int(fields[1]), (1...65535).contains(port) {
                                    self.announcedMode = mode
                                    self.announcedPort = port
                                }
                            }
                        }
                    }
                    self.lock.unlock()
                }
            }
        }
        process.terminationHandler = { child in
            _ = drained.wait(timeout: .now() + 1)
            self.lock.lock()
            let attached = child.terminationReason == .exit && child.terminationStatus == 0
                && self.announcedMode == .attached && self.announcedPort != nil
            self.lock.unlock()
            // A successful attachment to an already-running daemon UI exits 0.
            if !attached { self.failure("Web UI exited (status \(child.terminationStatus))", onFailure) }
        }
        do { try process.run() }
        catch {
            errorPipe.fileHandleForWriting.closeFile()
            outputPipe.fileHandleForWriting.closeFile()
            throw error
        }
        errorPipe.fileHandleForWriting.closeFile()
        outputPipe.fileHandleForWriting.closeFile()
        DispatchQueue.global().async {
            let deadline = Date().addingTimeInterval(timeout)
            while Date() < deadline {
                self.lock.lock()
                let port = self.announcedPort
                let failed = self.failed
                self.lock.unlock()
                if failed { return }
                if let port = port, probe(port),
                   self.permitsReady(process) {
                    self.lock.lock()
                    let announce = !self.failed && !self.ready
                    self.ready = announce
                    self.lock.unlock()
                    if announce { onReady(port) }
                    return
                }
                Thread.sleep(forTimeInterval: 0.1)
            }
            if process.isRunning { process.terminate() }
            self.failure("Web UI readiness timed out", onFailure)
        }
    }

    // A serving/supervising child must remain alive. Only an explicitly
    // attached child can exit successfully while the daemon retains the UI.
    func permitsReady(_ process: Process) -> Bool {
        lock.lock()
        let allowed = !failed
        let attached = announcedMode == .attached
        lock.unlock()
        return allowed && (process.isRunning ||
            (attached && process.terminationReason == .exit && process.terminationStatus == 0))
    }

    static func healthy(port: Int) -> Bool {
        guard let url = URL(string: "http://127.0.0.1:\(port)/api/v1/health") else { return false }
        var request = URLRequest(url: url)
        request.cachePolicy = .reloadIgnoringLocalCacheData
        request.timeoutInterval = 0.5
        let result = HealthResult()
        let complete = DispatchSemaphore(value: 0)
        let task = URLSession.shared.dataTask(with: request) { _, response, _ in
            result.lock.lock()
            result.healthy = (response as? HTTPURLResponse)?.statusCode == 200
            result.lock.unlock()
            complete.signal()
        }
        task.resume()
        guard complete.wait(timeout: .now() + 1) == .success else { task.cancel(); return false }
        result.lock.lock()
        defer { result.lock.unlock() }
        return result.healthy
    }
}
private final class HealthResult {
    let lock = NSLock()
    var healthy = false
}
