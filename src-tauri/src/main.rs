use std::fs::File;
use std::io::{Write, Read};
use std::path::Path;
use tauri::command;
use walkdir::WalkDir;
use zip::write::FileOptions;

// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    zip_compressor_lib::run()
}


