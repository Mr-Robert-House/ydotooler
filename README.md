
# ydotooler — A TUI Script Builder for ydotool

ydotooler is a terminal-based utility for building ydotool scripts without having to write them by hand. It provides a simple, interactive interface for assembling input events and exporting them as ready-to-run Bash scripts.

# Full disclosure

I am not a rust guy , I want to be but I'm not right now. I tried to write this in other languages, but it just didn't do everything I wanted it to do. So I ported what I already had over to rust and then added on top of that. Yes, I use Ai. I won't get into the argument if this counts as vibe coding or not. Me and several different LLM had several hours on this. I first started working on this in late 2025, this was not a rush ordeal with no debugging. 


# Why this exists

Coming from Windows, I was a huge fan of tools like Pulover’s Macro Creator. When switching to Linux, I was always looking for something with similar functionality.

I briefly used xdotool, but with Wayland becoming the default, that path became less viable. That led me to ydotool—which works great—but the surrounding ecosystem is still pretty sparse when it comes to user-friendly tooling.

So instead of waiting for something to appear, I built this and decided to share it to hopefully help grow the ecosystem a bit.

# What this tool does

ydotooler is not a replacement for ydotool. It is only a script generator.
It lets you:

* Build sequences of input events interactively
* Add:
  * Key presses/releases
  * Mouse button clicks (via key events)
  * Delays
  * Text typing
  * Loop blocks (including infinite loops)
* Reorder, edit, duplicate, and delete events
* Preview the generated Bash script in real time
* Save/load projects (.ydotooler format)
* Export executable .sh scripts ready to run

Under the hood, it generates standard ydotool commands like:

ydotool key 30:1 30:0 or sleep 0.1

# Requirements
* You must already have ydotool installed and properly configured for your system.This includes:
    * ydotoold running (preferably as a background service)
    * Proper permissions depending on your distro/setup

This tool does not handle any of that—it only generates scripts.

* keymap file
    * There is a default one already included, but if your input-event-codes.h is different from default then you will need to use my bash tool to create a new keymap.

# Usage workflow
* Launch ydotooler
* Build your event sequence in the TUI
* Save your project (optional)
* Generate the script
* Assign the script to a keyboard shortcut through your desktop environment

# Limitations / Known Issues
* Mouse movement is not implemented
    * I’ve never been able to get reliable mouse movement working with raw ydotool
    * It’s possible I’m overlooking something, but for now it’s intentionally excluded
* Relies entirely on ydotool behavior
    * Any quirks, delays, or inconsistencies come from ydotool itself
    * Running ydotoold as a persistent service is highly recommended

# Project format
Projects are saved as .ydotooler files, which store:

* Event sequences
* Auto-delay settings
* Loop configurations

These can be reloaded and edited later.

# Goal

This is mainly a quality-of-life tool for people using ydotool under Wayland. If it saves you from manually writing scripts or helps you prototype faster, it’s doing its job.
