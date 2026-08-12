package com.mycroc.app

import io.flutter.embedding.android.FlutterActivity

class MainActivity : FlutterActivity() {
    override fun onCreate(savedInstanceState: android.os.Bundle?) {
        super.onCreate(savedInstanceState)
        // Pre-create the app-scoped external files dir via the system API
        // (Context.getExternalFilesDir). The Rust bridge derives the same
        // path (<EXTERNAL_STORAGE>/Android/data/<pkg>/files) from the process
        // name, but raw mkdir/write of that subtree is denied by the
        // Android 11+ FUSE layer when the directory does not yet exist —
        // creating it through the platform API grants the app full ownership.
        getExternalFilesDir(null)
    }
}
