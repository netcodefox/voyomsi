import AppKit
import SwiftUI

/// VoyOMSI on a Mac: this app is the launcher (SwiftUI); the game is the
/// `voyomsi` program beside it in the bundle, which the launcher starts as a process of its
/// own for every drive, as on the other computers.
@main
struct VoyOMSIApp: App {
    @State private var store = LauncherStore()
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    init() {
        AppLanguage.apply(home: FileManager.default.homeDirectoryForCurrentUser)
        Theme.registerFonts()
    }

    var body: some Scene {
        WindowGroup("VoyOMSI") {
            LauncherRoot()
                .environment(store)
                .frame(minWidth: 1080, minHeight: 680)
                .preferredColorScheme(.dark)
                .onAppear {
                    Platform.restoreAccess()
                    store.open()
                }
        }
        .windowStyle(.hiddenTitleBar)
        .windowToolbarStyle(.unifiedCompact)
        .defaultSize(width: 1440, height: 880)
        .commands {
            CommandGroup(replacing: .newItem) {}
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}
