#!/usr/bin/env python3
"""Small native UI used by the Computer Use acceptance tests.

It deliberately exercises semantic controls, scrolling, dialogs, dynamic
content, a disabled control, a destructive confirmation, and a fake password
field. It does not contact the network and stores only the profile fixture.
"""

import json
import os
import tkinter as tk
from tkinter import messagebox, ttk


PROFILE_PATH = os.path.join(os.environ.get("TMPDIR", "/tmp"), "little-monkey-testapp-profile.json")


class TestApp:
    def __init__(self, root: tk.Tk) -> None:
        self.root = root
        self.root.title("Little Monkey TestApp")
        self.root.geometry("640x560")
        self.dark = tk.BooleanVar(value=False)
        self.profile = tk.StringVar(value="Test profile")
        self.status = tk.StringVar(value="Not saved")
        self.dynamic_count = 0

        menu = tk.Menu(root)
        file_menu = tk.Menu(menu, tearoff=False)
        file_menu.add_command(label="Save", command=self.save)
        file_menu.add_separator()
        file_menu.add_command(label="Quit", command=root.destroy)
        menu.add_cascade(label="File", menu=file_menu)
        preferences = tk.Menu(menu, tearoff=False)
        preferences.add_checkbutton(label="Dark mode", variable=self.dark, command=self.apply_theme)
        menu.add_cascade(label="Preferences", menu=preferences)
        root.config(menu=menu)

        outer = ttk.Frame(root, padding=16)
        outer.pack(fill="both", expand=True)
        ttk.Label(outer, text="Little Monkey TestApp", font=("TkDefaultFont", 18, "bold")).pack(anchor="w")
        ttk.Label(outer, text="Native semantic-control acceptance fixture").pack(anchor="w", pady=(0, 12))

        profile = ttk.LabelFrame(outer, text="Profile")
        profile.pack(fill="x", pady=4)
        ttk.Label(profile, text="Profile name").grid(row=0, column=0, sticky="w", padx=8, pady=8)
        ttk.Entry(profile, textvariable=self.profile, width=34).grid(row=0, column=1, sticky="ew", padx=8, pady=8)
        ttk.Button(profile, text="Save profile", command=self.save).grid(row=0, column=2, padx=8, pady=8)
        profile.columnconfigure(1, weight=1)
        ttk.Label(profile, textvariable=self.status).grid(row=1, column=0, columnspan=3, sticky="w", padx=8, pady=(0, 8))

        controls = ttk.LabelFrame(outer, text="Controls")
        controls.pack(fill="x", pady=4)
        ttk.Checkbutton(controls, text="Dark mode", variable=self.dark, command=self.apply_theme).grid(row=0, column=0, padx=8, pady=8, sticky="w")
        ttk.Button(controls, text="Open dialog", command=self.open_dialog).grid(row=0, column=1, padx=8, pady=8)
        ttk.Button(controls, text="Disabled button", state="disabled").grid(row=0, column=2, padx=8, pady=8)
        ttk.Button(controls, text="Add dynamic item", command=self.add_item).grid(row=1, column=0, padx=8, pady=8)
        ttk.Button(controls, text="Destructive action", command=self.destroy_action).grid(row=1, column=1, padx=8, pady=8)

        password = ttk.LabelFrame(outer, text="Fake password field (must be blocked)")
        password.pack(fill="x", pady=4)
        ttk.Label(password, text="Password").pack(side="left", padx=8, pady=8)
        ttk.Entry(password, show="*", name="fake-password").pack(side="left", fill="x", expand=True, padx=8, pady=8)

        listing = ttk.LabelFrame(outer, text="Scrollable list")
        listing.pack(fill="both", expand=True, pady=4)
        canvas = tk.Canvas(listing, highlightthickness=0)
        scrollbar = ttk.Scrollbar(listing, orient="vertical", command=canvas.yview)
        self.list_frame = ttk.Frame(canvas)
        self.list_frame.bind("<Configure>", lambda _: canvas.configure(scrollregion=canvas.bbox("all")))
        canvas.create_window((0, 0), window=self.list_frame, anchor="nw")
        canvas.configure(yscrollcommand=scrollbar.set)
        canvas.pack(side="left", fill="both", expand=True)
        scrollbar.pack(side="right", fill="y")
        for index in range(1, 31):
            ttk.Label(self.list_frame, text=f"List item {index}").pack(anchor="w", padx=10, pady=2)

        self.root.after(100, self.apply_theme)

    def save(self) -> None:
        with open(PROFILE_PATH, "w", encoding="utf-8") as handle:
            json.dump({"profile": self.profile.get(), "dark": self.dark.get()}, handle)
        self.status.set("Saved")

    def apply_theme(self) -> None:
        self.root.configure(bg="#202124" if self.dark.get() else "#f5f5f5")
        self.status.set("Dark mode enabled" if self.dark.get() else "Light mode enabled")

    def open_dialog(self) -> None:
        messagebox.showinfo("Test dialog", "Dialog opened successfully")

    def add_item(self) -> None:
        self.dynamic_count += 1
        ttk.Label(self.list_frame, text=f"Dynamic item {self.dynamic_count}").pack(anchor="w", padx=10, pady=2)

    def destroy_action(self) -> None:
        if messagebox.askyesno("Confirm destructive action", "Delete the test profile?"):
            try:
                os.remove(PROFILE_PATH)
            except FileNotFoundError:
                pass
            self.status.set("Profile deleted")


if __name__ == "__main__":
    root = tk.Tk()
    TestApp(root)
    root.mainloop()
