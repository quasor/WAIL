// Wails v3 adapter: provides a Tauri-compatible API surface so main.js works unchanged.
// This must be loaded BEFORE main.js.
//
// Tauri API:
//   window.__TAURI__.core.invoke("command_name", { arg1: val1 })
//   window.__TAURI__.event.listen("event:name", callback)
//   window.__TAURI__.app.getVersion()
//
// Wails v3 API:
//   wails.Call.ByName("main.App.MethodName", arg1, arg2, ...)
//   wails.Events.On("event:name", callback)

(function() {
    'use strict';

    // The Wails v3 runtime is injected via WebViewDidFinishNavigation, which fires
    // AFTER all page scripts have executed. We need to wait for it before making calls.
    let _wailsReady = null;
    const wailsReady = new Promise(resolve => { _wailsReady = resolve; });

    function pollForWails() {
        if (typeof wails !== 'undefined' && wails.Call) {
            console.log('[wails-adapter] Wails runtime detected');
            _wailsReady();
        } else {
            setTimeout(pollForWails, 10);
        }
    }
    pollForWails();

    // Map Tauri snake_case command names to Wails PascalCase method names on App.
    const commandMap = {
        'list_public_rooms': 'main.App.ListPublicRooms',
        'join_room': 'main.App.JoinRoom',
        'disconnect': 'main.App.Disconnect',
        'change_bpm': 'main.App.ChangeBPM',
        'send_chat': 'main.App.SendChat',
        'set_test_tone': 'main.App.SetTestTone',
        'set_telemetry': 'main.App.SetTelemetry',
        'set_log_sharing': 'main.App.SetLogSharing',
        'get_default_recording_dir': 'main.App.GetDefaultRecordingDir',
        'cleanup_recordings': 'main.App.CleanupRecordings',
        'get_active_session': 'main.App.GetActiveSession',
        'get_plugin_install_errors': 'main.App.GetPluginInstallErrors',
        'rename_stream': 'main.App.RenameStream',
    };

    // Tauri invoke passes a single object of named args.
    // Wails Call.ByName takes positional args matching the Go method signature.
    // This mapping converts named args to positional for each command.
    const argOrder = {
        'join_room': ['room', 'password', 'displayName', 'bpm', 'bars', 'quantum',
                       'recordingEnabled', 'recordingDirectory', 'recordingStems',
                       'recordingRetentionDays', 'streamCount', 'testMode'],
        'change_bpm': ['bpm'],
        'send_chat': ['text'],
        'set_test_tone': ['streamIndex'],
        'set_telemetry': ['enabled'],
        'set_log_sharing': ['enabled'],
        'cleanup_recordings': ['directory', 'retentionDays'],
        'rename_stream': ['streamIndex', 'name'],
    };

    async function invoke(command, args) {
        const wailsMethod = commandMap[command];
        if (!wailsMethod) {
            console.warn('[wails-adapter] Unknown command:', command);
            throw new Error('Unknown command: ' + command);
        }

        // Wait for Wails runtime to be injected
        await wailsReady;

        // Convert named args to positional
        const order = argOrder[command];
        let positionalArgs;
        if (order && args) {
            positionalArgs = order.map(key => args[key] !== undefined ? args[key] : null);
        } else if (args) {
            positionalArgs = Object.values(args);
        } else {
            positionalArgs = [];
        }

        try {
            return await wails.Call.ByName(wailsMethod, ...positionalArgs);
        } catch (err) {
            // Wails wraps errors; extract the message
            const msg = typeof err === 'string' ? err : (err.message || String(err));
            throw msg;
        }
    }

    function listen(eventName, callback) {
        // Queue listener registration until Wails runtime is available
        wailsReady.then(() => {
            wails.Events.On(eventName, function(event) {
                // Wails event.data is the payload directly (not an array)
                callback({ payload: event.data });
            });
        });
        // Return a promise that resolves to a no-op unlisten (simplified)
        return Promise.resolve(() => {});
    }

    // Provide __TAURI__ compatibility
    window.__TAURI__ = {
        core: { invoke },
        event: { listen },
        app: {
            getVersion: function() {
                return Promise.resolve('2.0.0-go');
            }
        }
    };

    console.log('[wails-adapter] Tauri API shim loaded (waiting for Wails runtime)');
})();
