pluginManagement {
    val flutterSdkPath =
        run {
            val properties = java.util.Properties()
            file("local.properties").inputStream().use { properties.load(it) }
            val flutterSdkPath = properties.getProperty("flutter.sdk")
            require(flutterSdkPath != null) { "flutter.sdk not set in local.properties" }
            flutterSdkPath
        }

    includeBuild("$flutterSdkPath/packages/flutter_tools/gradle")

    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

plugins {
    id("dev.flutter.flutter-plugin-loader") version "1.0.0"
    // T20: AGP 8.11.1 + Kotlin 2.2.20 (drift ref combo) instead of the 3.44
    // template's AGP 9.0.1: file_picker 11.0.3 (latest stable) conditionally
    // skips KGP under AGP9 while Flutter's KGP-detection regex still sees it
    // as declared -> its .kt sources never compile -> app javac fails on the
    // registrant. Under AGP 8 the plugin applies its own KGP and everything
    // resolves (verified: this exact combo builds drift's app with cargokit).
    id("com.android.application") version "8.11.1" apply false
    id("org.jetbrains.kotlin.android") version "2.2.20" apply false
}

include(":app")
