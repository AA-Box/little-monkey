#!/usr/bin/env python3
"""GTK3 fixture used by the Linux/X11 AT-SPI acceptance lane."""

import json
import os

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk


PROFILE_PATH = os.path.join(os.environ.get("TMPDIR", "/tmp"), "little-monkey-testapp-profile.json")


class TestApp:
    def __init__(self, application) -> None:
        self.application = application
        self.root = Gtk.ApplicationWindow(application=application)
        self.root.set_title("Little Monkey TestApp")
        self.root.set_default_size(640, 560)
        self.root.connect("destroy", lambda *_args: self.application.quit())
        self.dark = Gtk.CheckButton(label="Dark mode")
        self.profile = Gtk.Entry()
        self.profile.set_text("Test profile")
        self.status = Gtk.Label(label="Not saved")
        self.dynamic_items = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        self.load_profile()

        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        outer.set_border_width(16)
        self.root.add(outer)
        outer.pack_start(Gtk.Label(label="Little Monkey TestApp"), False, False, 0)
        outer.pack_start(Gtk.Label(label="Native semantic-control acceptance fixture"), False, False, 0)

        profile_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        profile_box.pack_start(Gtk.Label(label="Profile name"), False, False, 0)
        profile_box.pack_start(self.profile, True, True, 0)
        save = Gtk.Button(label="Save profile")
        save.connect("clicked", self.save)
        profile_box.pack_start(save, False, False, 0)
        outer.pack_start(profile_box, False, False, 0)
        outer.pack_start(self.status, False, False, 0)

        controls = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        controls.pack_start(self.dark, False, False, 0)
        self.dark.connect("toggled", self.apply_theme)
        controls.pack_start(Gtk.Button(label="Open dialog"), False, False, 0)
        disabled = Gtk.Button(label="Disabled button")
        disabled.set_sensitive(False)
        controls.pack_start(disabled, False, False, 0)
        add_dynamic = Gtk.Button(label="Add dynamic item")
        add_dynamic.connect("clicked", self.add_item)
        controls.pack_start(add_dynamic, False, False, 0)
        controls.pack_start(Gtk.Button(label="Destructive action"), False, False, 0)
        outer.pack_start(controls, False, False, 0)

        password = Gtk.Entry()
        password.set_visibility(False)
        password.set_placeholder_text("Fake password field (must be blocked)")
        password.get_accessible().set_name("Fake password field (must be blocked)")
        outer.pack_start(password, False, False, 0)
        outer.pack_start(self.dynamic_items, True, True, 0)
        for index in range(1, 31):
            self.dynamic_items.pack_start(Gtk.Label(label=f"List item {index}"), False, False, 0)

        self.root.connect("map", self.open_secondary_window)
        self.root.show_all()

    def open_secondary_window(self, *_args) -> None:
        if hasattr(self, "secondary"):
            return
        self.secondary = Gtk.Window(title="Little Monkey TestApp Secondary")
        self.secondary.set_default_size(320, 180)
        self.secondary.set_transient_for(self.root)
        self.secondary.add(Gtk.Label(label="Little Monkey TestApp Secondary"))
        self.secondary.show_all()
        self.root.present()

    def load_profile(self) -> None:
        try:
            with open(PROFILE_PATH, encoding="utf-8") as handle:
                saved = json.load(handle)
            if isinstance(saved.get("profile"), str):
                self.profile.set_text(saved["profile"])
            if isinstance(saved.get("dark"), bool):
                self.dark.set_active(saved["dark"])
        except (OSError, json.JSONDecodeError, AttributeError):
            pass

    def save(self, *_args) -> None:
        with open(PROFILE_PATH, "w", encoding="utf-8") as handle:
            json.dump({"profile": self.profile.get_text(), "dark": self.dark.get_active()}, handle)
        self.status.set_text("Saved")

    def apply_theme(self, *_args) -> None:
        self.status.set_text("Dark mode enabled" if self.dark.get_active() else "Light mode enabled")

    def add_item(self, *_args) -> None:
        self.dynamic_items.pack_start(Gtk.Label(label="Dynamic item 1"), False, False, 0)
        self.dynamic_items.show_all()


if __name__ == "__main__":
    GLib.set_prgname("Little Monkey TestApp")
    application = Gtk.Application(application_id="com.aabox.LittleMonkeyTestApp")
    fixture = {}

    def activate(app) -> None:
        fixture["app"] = TestApp(app)

    application.connect("activate", activate)
    application.run(None)
