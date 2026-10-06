// Run from the repository root:
// swiftc app/Sources/DatabaseSelection.swift app/Tests/DatabaseSelectionTests.swift -o /tmp/DatabaseSelectionTests
// /tmp/DatabaseSelectionTests
import Foundation

// Removing publication discovery, path normalization, or config forwarding must
// fail these fixtures at the child-process boundary.
@main
struct DatabaseSelectionTests {
    static func main() throws {
        let fm = FileManager.default
        let root = fm.temporaryDirectory.appendingPathComponent("nw-selection-\(UUID().uuidString)")
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: root) }
        var failures = 0
        var checks = 0
        func check(_ condition: Bool, _ name: String) {
            checks += 1
            if !condition { failures += 1; print("FAIL: \(name)") }
        }
        func write(_ path: String, _ text: String = "") throws {
            try fm.createDirectory(atPath: (path as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
            try text.write(toFile: path, atomically: true, encoding: .utf8)
        }
        func home(_ name: String) throws -> String {
            let path = root.appendingPathComponent(name).path
            try fm.createDirectory(atPath: path, withIntermediateDirectories: true)
            return path
        }
        func select(_ home: String, _ env: [String: String] = [:]) -> DatabaseSelection? {
            DatabaseSelector.select(environment: env, home: home)
        }
        let globalHome = try home("global")
        let db = globalHome + "/brain.lbug"
        let config = globalHome + "/.nestweaver/instance.toml"
        try write(db + ".publications/CURRENT", "slot-a\n")
        try write(config, "db = '\(db)'\n")
        let global = select(globalHome)
        check(global?.databasePath == db, "global config discovers publication-only logical DB")
        check(global?.configPath == config, "global config retained")
        check(global?.daemonArguments == ["daemon", "--db", db, "start", "--config", config], "daemon receives logical DB and selected config")
        check(global?.uiArguments(port: 9377) == ["ui", "--port", "9377", "--no-open", "--db", db, "--config", config], "UI receives same logical DB and config")
        let ordinaryHome = try home("ordinary")
        let ordinaryDB = ordinaryHome + "/.local/share/nestweaver/a/brain.lbug"
        try write(ordinaryDB)
        check(select(ordinaryHome)?.databasePath == ordinaryDB, "ordinary DB fallback")
        check(select(ordinaryHome)?.configPath == nil, "missing global config omitted")
        check(select(ordinaryHome)?.daemonArguments == ["daemon", "--db", ordinaryDB, "start"], "daemon has no nonexistent config argument")
        check(select(ordinaryHome)?.uiArguments(port: 1234) == ["ui", "--port", "1234", "--no-open", "--db", ordinaryDB], "UI has no nonexistent config argument")
        let fallbackHome = try home("fallback")
        let fallbackDB = fallbackHome + "/.local/share/nestweaver/z/brain.lbug"
        try write(fallbackDB + ".publications/CURRENT", "invalid marker is CLI's responsibility")
        check(select(fallbackHome)?.databasePath == fallbackDB, "publication glob fallback keeps logical path")
        check(select(try home("empty")) == nil, "no DB returns nil")
        check(select(globalHome, ["NESTWEAVER_DB": "~/override.lbug"])?.databasePath == globalHome + "/override.lbug", "explicit DB override and injected tilde home")
        check(select(globalHome, ["NESTWEAVER_DB": "~/override.lbug"])?.configPath == config, "DB override retains existing global config")
        let aliasHome = (try home("publication-alias") as NSString).resolvingSymlinksInPath
        let aliasDB = aliasHome + "/alias.lbug"
        let aliasConfig = aliasHome + "/.nestweaver/instance.toml"
        let slotA = "11111111-1111-4111-8111-111111111111"
        let slotB = "22222222-2222-4222-8222-222222222222"
        let graphA = aliasDB + ".publications/slots/" + slotA + "/graph.lbug"
        let graphB = aliasDB + ".publications/slots/" + slotB + "/graph.lbug"
        try write(graphA, "fixture graph A")
        try write(graphB, "fixture graph B")
        try fm.createSymbolicLink(atPath: aliasDB, withDestinationPath: graphA)
        try write(aliasDB + ".publications/CURRENT", slotA + "\n")
        try write(aliasConfig, "db = '\(aliasDB)'\n")
        check(fm.fileExists(atPath: graphA) && fm.fileExists(atPath: graphB)
              && (aliasDB as NSString).resolvingSymlinksInPath == graphA,
              "publication alias fixture has two physical graphs and symlink to slot A")
        let aliasSelection = select(aliasHome)
        check(aliasSelection?.databasePath == aliasDB, "config publication alias retains logical DB name")
        check(aliasSelection?.daemonArguments == ["daemon", "--db", aliasDB, "start", "--config", aliasConfig], "daemon receives publication alias without physical slot UUID")
        check(aliasSelection?.uiArguments(port: 9377) == ["ui", "--port", "9377", "--no-open", "--db", aliasDB, "--config", aliasConfig], "UI receives publication alias without physical slot UUID")
        check(select(aliasHome, ["NESTWEAVER_DB": "~/alias.lbug"])?.databasePath == aliasDB, "explicit DB override retains publication alias after tilde expansion")
        try write(aliasDB + ".publications/CURRENT", slotB + "\n")
        check(select(aliasHome)?.databasePath == aliasDB, "CURRENT rotation retains logical alias despite symlink still pointing to slot A")
        let aliasFallbackHome = (try home("publication-alias-fallback") as NSString).resolvingSymlinksInPath
        let aliasFallbackDB = aliasFallbackHome + "/.local/share/nestweaver/a/brain.lbug"
        try write(aliasFallbackDB + ".publications/slots/" + slotA + "/graph.lbug", "fixture graph A")
        try fm.createSymbolicLink(atPath: aliasFallbackDB,
                                  withDestinationPath: aliasFallbackDB + ".publications/slots/" + slotA + "/graph.lbug")
        try write(aliasFallbackDB + ".publications/CURRENT", slotA + "\n")
        check(select(aliasFallbackHome)?.databasePath == aliasFallbackDB, "glob fallback retains publication alias")
        let ordinaryAlias = aliasHome + "/ordinary-alias.lbug"
        let ordinaryTarget = aliasHome + "/ordinary-target.lbug"
        try write(ordinaryTarget)
        try fm.createSymbolicLink(atPath: ordinaryAlias, withDestinationPath: ordinaryTarget)
        try write(aliasConfig, "db = '\(ordinaryAlias)'\n")
        check(select(aliasHome)?.databasePath == ordinaryTarget, "ordinary DB symlink without CURRENT remains canonical")
        let explicit = globalHome + "/selected.toml"
        try write(explicit, "db = '~/brain.lbug' # comment\n")
        check(select(globalHome, ["NESTWEAVER_CONFIG": "~/selected.toml"])?.configPath == explicit, "explicit config wins over global")
        check(select(globalHome, ["NESTWEAVER_CONFIG": "~/selected.toml"])?.databasePath == db, "config DB tilde and trailing comment")
        check(select(ordinaryHome, ["NESTWEAVER_CONFIG": "~/missing.toml"])?.configPath == nil, "nonexistent explicit config omitted")
        let target = globalHome + "/actual/config.toml"
        let relativeDB = globalHome + "/actual/relative.lbug"
        try write(relativeDB + ".publications/CURRENT", "slot-a")
        try write(target, "db = 'relative.lbug'\n")
        let link = globalHome + "/linked.toml"
        try fm.createSymbolicLink(atPath: link, withDestinationPath: target)
        let linked = select(globalHome, ["NESTWEAVER_CONFIG": link])
        check(linked?.databasePath == relativeDB, "relative DB uses symlink target config directory")
        check(linked?.configPath == target, "canonical selected config forwarded")
        let parserHome = try home("parser")
        let parserConfig = parserHome + "/.nestweaver/instance.toml"
        let quotedDB = parserHome + "/brain#=\"quoted.lbug"
        try write(quotedDB)
        try write(parserConfig, "# db = 'wrong'\nlabel = \"\"\"\n[bogus]\ndb = 'wrong'\n\"\"\"\ndb = \"\(parserHome)/brain#=\\\"quoted.lbug\" # suffix\n[section]\ndb = 'wrong'\n")
        check(select(parserHome)?.databasePath == quotedDB, "multiline text, embedded comment/equal/quote and sections do not confuse db")
        try write(parserConfig, "[section]\ndb = '\(quotedDB)'\n")
        check(select(parserHome) == nil, "section db never treated as top-level db")
        try write(parserConfig, "db = '\(quotedDB)' # literal path\n")
        check(select(parserHome)?.databasePath == quotedDB, "literal path with trailing comment")
        // CRLF is one Swift Character; losing its boundary can hide db after a
        // comment/key or accidentally accept an invalid multiline basic path.
        try write(parserConfig, "# leading comment\r\ndb = '\(quotedDB)'\r\n")
        check(select(parserHome)?.databasePath == quotedDB, "CRLF leading comment ends before db")
        try write(parserConfig, "instance_id = 'fixture'\r\ndb = '\(quotedDB)'\r\n")
        check(select(parserHome)?.databasePath == quotedDB, "CRLF preceding key does not absorb db")
        try write(parserConfig, "db = '\(quotedDB)'\r\n[section]\r\ndb = 'wrong'\r\n")
        check(select(parserHome)?.databasePath == quotedDB, "CRLF section boundary preserves top-level db")
        try write(parserConfig, "# leading comment\r\n[section]\r\ndb = '\(quotedDB)'\r\n")
        check(select(parserHome) == nil, "CRLF section db stays excluded")
        try write(parserConfig, "instance_id = 'fixture' # preceding comment\r\ndb = \"\(parserHome)/brain#=\\\"quoted.lbug\" # trailing comment\r\n")
        check(select(parserHome)?.databasePath == quotedDB, "CRLF quoted path preserves hash, equals and escaped quote")
        let invalidMultilineDB = parserHome + "/line\r\nbreak.lbug"
        try write(invalidMultilineDB)
        try write(parserConfig, "db = '\(invalidMultilineDB)'\r\n")
        check(select(parserHome) == nil, "raw CRLF inside single-line string cannot select a path")
        print("\(checks - failures)/\(checks) checks passed")
        if failures > 0 { exit(1) }
    }
}
