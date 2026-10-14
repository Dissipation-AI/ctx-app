// Lists on-screen windows: owner pid, width, height (points), title. Used by run.sh to check the
// window under test is really showing, at the size asked for.
import CoreGraphics
import Foundation

let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
for w in list where (w[kCGWindowLayer as String] as? Int) == 0 {
    let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
    let pid = w[kCGWindowOwnerPID as String] as? Int ?? 0
    let width = (b["Width"] as? Double).map { Int($0) } ?? 0
    let height = (b["Height"] as? Double).map { Int($0) } ?? 0
    print(pid, width, height, w[kCGWindowName as String] as? String ?? "", separator: "\t")
}
