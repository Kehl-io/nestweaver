import Foundation

@main
struct UIChildReadinessTests {
    static func main() throws {
        func child(_ command: String) -> Process {
            let process = Process()
            process.executableURL = URL(fileURLWithPath: "/bin/sh")
            process.arguments = ["-c", command]
            return process
        }
        let exited = child("echo strict-config-mismatch >&2; exit 64")
        let failure = DispatchSemaphore(value: 0)
        let observer = UIChildReadiness()
        let lock = NSLock()
        var announced = false
        var diagnostic = ""
        try observer.launch(exited, timeout: 2, probe: { _ in false }, onReady: { _ in
            lock.lock(); announced = true; lock.unlock()
        }, onFailure: { message in
            lock.lock(); diagnostic = message; lock.unlock(); failure.signal()
        })
        precondition(failure.wait(timeout: .now() + 4) == .success, "early exit observed")
        exited.waitUntilExit()
        lock.lock()
        precondition(!announced && diagnostic.contains("strict-config-mismatch") && diagnostic.contains("64"))
        lock.unlock()

        let timeoutChild = child("exec sleep 10")
        let timedOut = DispatchSemaphore(value: 0)
        let timeoutObserver = UIChildReadiness()
        try timeoutObserver.launch(timeoutChild, timeout: 0.2, probe: { _ in false },
            onReady: { _ in preconditionFailure("unready child announced") },
            onFailure: { _ in timedOut.signal() })
        precondition(timedOut.wait(timeout: .now() + 4) == .success)
        timeoutChild.waitUntilExit()
        precondition(!timeoutChild.isRunning, "timeout terminates child")

        let live = child("echo NW_UI_READY:$NESTWEAVER_LAUNCHER_READY_TOKEN:supervised:9377; exec sleep 10")
        let ready = DispatchSemaphore(value: 0)
        let stopped = DispatchSemaphore(value: 0)
        let liveObserver = UIChildReadiness()
        let started = Date()
        try liveObserver.launch(live, timeout: 3,
            probe: { port in port == 9377 && Date().timeIntervalSince(started) > 0.3 },
            onReady: { port in precondition(port == 9377); ready.signal() }, onFailure: { _ in stopped.signal() })
        precondition(ready.wait(timeout: .now() + 0.1) == .timedOut, "wait for listener proof")
        precondition(ready.wait(timeout: .now() + 3) == .success)
        live.terminate()
        precondition(stopped.wait(timeout: .now() + 4) == .success, "post-ready exit observed")
        live.waitUntilExit()
        // An unrelated healthy listener cannot satisfy readiness without the
        // token, even while the child is alive and eventually exits in failure.
        let foreign = child("sleep 0.3; echo rejected >&2; exit 64")
        let rejected = DispatchSemaphore(value: 0)
        let foreignObserver = UIChildReadiness()
        try foreignObserver.launch(foreign, timeout: 2, probe: { _ in true },
            onReady: { _ in preconditionFailure("foreign listener accepted") },
            onFailure: { _ in rejected.signal() })
        precondition(rejected.wait(timeout: .now() + 4) == .success)
        foreign.waitUntilExit()
        for mode in ["direct", "supervised"] {
            let owned = child("echo NW_UI_READY:$NESTWEAVER_LAUNCHER_READY_TOKEN:\(mode):9378; read release; exit 0")
            let input = Pipe()
            owned.standardInput = input
            let ownedReady = DispatchSemaphore(value: 0)
            let ownedExited = DispatchSemaphore(value: 0)
            let ownedObserver = UIChildReadiness()
            try ownedObserver.launch(owned, timeout: 2, probe: { $0 == 9378 },
                onReady: { _ in ownedReady.signal() },
                onFailure: { message in precondition(message.contains("status 0")); ownedExited.signal() })
            precondition(ownedReady.wait(timeout: .now() + 3) == .success, "owned child must become ready")
            input.fileHandleForWriting.write(Data("release\n".utf8))
            input.fileHandleForWriting.closeFile()
            precondition(ownedExited.wait(timeout: .now() + 3) == .success, "owned exit zero must fail")
            owned.waitUntilExit()
            precondition(!ownedObserver.permitsReady(owned), "exited owned child cannot remain ready")
        }
        let attached = child("echo NW_UI_READY:$NESTWEAVER_LAUNCHER_READY_TOKEN:attached:1234; exit 0")
        let attachReady = DispatchSemaphore(value: 0)
        let attachObserver = UIChildReadiness()
        try attachObserver.launch(attached, timeout: 2, probe: { $0 == 1234 },
            onReady: { port in precondition(port == 1234); attachReady.signal() },
            onFailure: { _ in preconditionFailure("verified daemon attachment rejected") })
        precondition(attachReady.wait(timeout: .now() + 4) == .success)
        attached.waitUntilExit()
        print("UI child startup, ownership, attachment, timeout, and termination fixtures passed")
    }
}
