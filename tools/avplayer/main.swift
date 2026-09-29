// AVQueuePlayer check: the playback engine Arpeggi uses (iOS and macOS share AVFoundation).
//
// usage: avplayer-check <url1> <url2> [expected duration of url1 in seconds]
//
// Verifies what broke with DASH-fragmented files in the first proxy: the reported
// duration matches, the first item plays to its real end, and the queue advances
// to the second item, which starts playing.
import AVFoundation
import Foundation

struct Result: Codable {
    var duration: Double = -1
    var expected: Double? = nil
    var reachedEnd = false
    var advanced = false
    var secondPlaying = false
    var error: String? = nil
    var ok: Bool { error == nil && reachedEnd && advanced && secondPlaying && durationOK }
    var durationOK: Bool { expected == nil || abs(duration - expected!) <= 2 }
}

let args = CommandLine.arguments
guard args.count >= 3, let first = URL(string: args[1]), let second = URL(string: args[2]) else {
    FileHandle.standardError.write("usage: avplayer-check <url1> <url2> [expected-seconds]\n".data(using: .utf8)!)
    exit(2)
}
var result = Result()
result.expected = args.count > 3 ? Double(args[3]) : nil

let item1 = AVPlayerItem(url: first)
let item2 = AVPlayerItem(url: second)
let player = AVQueuePlayer(items: [item1, item2])
player.volume = 0
player.automaticallyWaitsToMinimizeStalling = false

func wait(_ seconds: Double, until condition: () -> Bool) -> Bool {
    let deadline = Date().addingTimeInterval(seconds)
    while Date() < deadline {
        if condition() { return true }
        RunLoop.current.run(until: Date().addingTimeInterval(0.05))
    }
    return condition()
}

var ended = false
let token = NotificationCenter.default.addObserver(
    forName: .AVPlayerItemDidPlayToEndTime, object: item1, queue: .main) { _ in ended = true }

if !wait(30, until: { item1.status != .unknown }) || item1.status != .readyToPlay {
    result.error = "item1 not ready: \(item1.error?.localizedDescription ?? "timeout")"
} else {
    result.duration = CMTimeGetSeconds(item1.duration)
    let target = CMTime(seconds: max(0, result.duration - 4), preferredTimescale: 600)
    var seeked = false
    player.seek(to: target, toleranceBefore: .zero, toleranceAfter: .zero) { _ in seeked = true }
    _ = wait(15, until: { seeked })
    player.play()
    result.reachedEnd = wait(20, until: { ended })
    result.advanced = wait(10, until: { player.currentItem === item2 })
    result.secondPlaying = wait(20, until: {
        player.currentItem === item2 && item2.status == .readyToPlay && CMTimeGetSeconds(item2.currentTime()) > 0.5
    })
    if !result.reachedEnd {
        result.error = "item1 never reached its end (position \(CMTimeGetSeconds(item1.currentTime())) of \(result.duration))"
    }
}
NotificationCenter.default.removeObserver(token)
let encoder = JSONEncoder()
encoder.outputFormatting = .sortedKeys
print(String(data: try! encoder.encode(result), encoding: .utf8)!)
exit(result.ok ? 0 : 1)
