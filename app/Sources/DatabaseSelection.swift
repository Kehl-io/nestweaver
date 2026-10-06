import Foundation

/// Keep the logical database and its configuration together for both children.
struct DatabaseSelection {
    let databasePath: String
    let configPath: String?

    private var configArguments: [String] {
        configPath.map { ["--config", $0] } ?? []
    }

    var daemonArguments: [String] {
        ["daemon", "--db", databasePath, "start"] + configArguments
    }

    func uiArguments(port: Int) -> [String] {
        ["ui", "--port", String(port), "--no-open", "--db", databasePath] + configArguments
    }
}

enum DatabaseSelector {
    static func select(environment: [String: String], home: String,
                       fileManager: FileManager = .default) -> DatabaseSelection? {
        func resolve(_ path: String, relativeTo directory: String) -> String {
            let expanded = path.hasPrefix("~/") ? home + String(path.dropFirst()) : path
            let absolute = expanded.hasPrefix("/") ? expanded : directory + "/" + expanded
            return (absolute as NSString).resolvingSymlinksInPath
        }
        func exists(_ logical: String) -> Bool {
            // CURRENT is only a discovery signal. The CLI validates its contents
            // and resolves the published slot; children always receive logical.
            fileManager.fileExists(atPath: logical)
                || fileManager.fileExists(atPath: logical + ".publications/CURRENT")
        }
        let explicitConfig = environment["NESTWEAVER_CONFIG"].flatMap { $0.isEmpty ? nil : $0 }
        let candidateConfig = resolve(explicitConfig ?? home + "/.nestweaver/instance.toml",
                                      relativeTo: fileManager.currentDirectoryPath)
        let config = fileManager.fileExists(atPath: candidateConfig) ? candidateConfig : nil
        if let db = environment["NESTWEAVER_DB"], !db.isEmpty {
            return DatabaseSelection(databasePath: resolve(db, relativeTo: fileManager.currentDirectoryPath),
                                     configPath: config)
        }
        if let config = config,
           let contents = try? String(contentsOfFile: config, encoding: .utf8),
           let db = topLevelDatabase(contents) {
            let logical = resolve(db, relativeTo: (config as NSString).deletingLastPathComponent)
            if exists(logical) { return DatabaseSelection(databasePath: logical, configPath: config) }
        }
        let nestDir = home + "/.local/share/nestweaver"
        if let dirs = try? fileManager.contentsOfDirectory(atPath: nestDir) {
            for dir in dirs.sorted() {
                let logical = resolve(nestDir + "/" + dir + "/brain.lbug", relativeTo: home)
                if exists(logical) { return DatabaseSelection(databasePath: logical, configPath: config) }
            }
        }
        return nil
    }

    /// A small lexical reader, not a TOML validator. Only a top-level string db
    /// is used. Track strings across lines so text inside multiline strings can
    /// never masquerade as a key or section. The CLI owns full config validation.
    private static func topLevelDatabase(_ content: String) -> String? {
        let chars = Array(content)
        var statements: [String] = []
        var buffer = ""
        var quote: Character?
        var multiline = false
        var index = 0
        while index < chars.count {
            let char = chars[index]
            if let delimiter = quote {
                if char == "\\", delimiter == "\"", index + 1 < chars.count {
                    buffer.append(char)
                    buffer.append(chars[index + 1])
                    index += 2
                    continue
                }
                if char == delimiter {
                    if multiline {
                        if index + 2 < chars.count, chars[index + 1] == delimiter, chars[index + 2] == delimiter {
                            buffer += String(repeating: String(delimiter), count: 3)
                            index += 3
                            quote = nil
                            continue
                        }
                    } else { quote = nil }
                }
                buffer.append(char)
            } else if char == "#" {
                while index < chars.count, chars[index] != "\n", chars[index] != "\r\n" { index += 1 }
                continue
            } else if char == "\n" || char == "\r\n" {
                // Swift treats CRLF as a single Character. Preserve string
                // contents above, but recognize both TOML line endings here.
                statements.append(buffer)
                buffer = ""
            } else if char == "\"" || char == "'" {
                quote = char
                multiline = index + 2 < chars.count && chars[index + 1] == char && chars[index + 2] == char
                if multiline {
                    buffer += String(repeating: String(char), count: 3)
                    index += 3
                    continue
                }
                buffer.append(char)
            } else { buffer.append(char) }
            index += 1
        }
        if quote == nil { statements.append(buffer) }
        for statement in statements {
            let line = statement.trimmingCharacters(in: .whitespacesAndNewlines)
            // TOML does not return to the root table after a section starts.
            if line.hasPrefix("[") { return nil }
            guard let equal = line.firstIndex(of: "=") else { continue }
            let key = line[..<equal].trimmingCharacters(in: .whitespaces)
            guard key == "db" || stringValue(key) == "db" else { continue }
            return stringValue(String(line[line.index(after: equal)...]).trimmingCharacters(in: .whitespacesAndNewlines))
        }
        return nil
    }

    private static func stringValue(_ value: String) -> String? {
        let chars = Array(value)
        guard chars.count >= 2, let delimiter = chars.first,
              delimiter == "\"" || delimiter == "'", chars.last == delimiter else { return nil }
        // Multiline strings are deliberately not interpreted as database paths.
        if value.hasPrefix("\"\"\"") || value.hasPrefix("'''") { return nil }
        var result = ""
        var index = 1
        while index < chars.count - 1 {
            let char = chars[index]
            if char == delimiter || char == "\n" || char == "\r" || char == "\r\n" { return nil }
            if char == "\\", delimiter == "\"" {
                index += 1
                guard index < chars.count - 1 else { return nil }
                switch chars[index] {
                case "b": result.append("\u{8}")
                case "t": result.append("\t")
                case "n": result.append("\n")
                case "f": result.append("\u{c}")
                case "r": result.append("\r")
                case "\"": result.append("\"")
                case "\\": result.append("\\")
                case "u", "U":
                    let count = chars[index] == "u" ? 4 : 8
                    guard index + count < chars.count - 1,
                          let code = UInt32(String(chars[(index + 1)...(index + count)]), radix: 16),
                          let scalar = UnicodeScalar(code) else { return nil }
                    result.unicodeScalars.append(scalar)
                    index += count
                default: return nil
                }
            } else { result.append(char) }
            index += 1
        }
        return result.isEmpty ? nil : result
    }
}
