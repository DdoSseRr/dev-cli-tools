use std::fs;
use std::path::{Path, PathBuf};
use std::io::{self, BufRead, Write};
use std::fs::File;
use std::collections::HashSet;
use std::sync::Arc;
use clap::Parser;
use trash;
use rayon::prelude::*;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use console::style;
use std::sync::mpsc;
use std::thread;
use walkdir::WalkDir;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Путь к файлу со списком нежелательных папок
    #[arg(short = 'c', long = "config")]
    config_path: Option<String>,

    /// Корневой путь для поиска
    #[arg(short = 'r', long = "root")]
    root_path: Option<String>,
}

fn load_unwanted_folders(file_path: &str) -> io::Result<HashSet<String>> {
    let file = File::open(file_path)?;
    let reader = io::BufReader::new(file);

    let mut folders = HashSet::new();
    for line in reader.lines() {
        let line = line?;
        folders.insert(line.trim().to_string());
    }
    Ok(folders)
}

fn collect_unwanted_directories(root_path: &Path, unwanted_folders: &HashSet<String>) -> io::Result<Vec<PathBuf>> {
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap()
    );
    
    let mut dirs_to_delete = Vec::new();
    let mut total_scanned = 0;
    
    // Используем WalkDir для эффективного обхода директорий в одном потоке
    for entry in WalkDir::new(root_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !e.file_name().to_str().map_or(false, |s| s.starts_with('.')))
    {
        match entry {
            Ok(entry) => {
                total_scanned += 1;
                if total_scanned % 100 == 0 { // Обновляем статус каждые 100 файлов
                    spinner.set_message(format!("Просканировано: {} | Найдено для удаления: {}", 
                        style(total_scanned).cyan(),
                        style(dirs_to_delete.len()).yellow()));
                }
                
                if entry.file_type().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        if unwanted_folders.contains(name) {
                            dirs_to_delete.push(entry.path().to_path_buf());
                        }
                    }
                }
            }
            Err(e) => eprintln!("Ошибка при сканировании: {}", e),
        }
    }

    spinner.finish_with_message(format!("Сканирование завершено. Найдено директорий для удаления: {}", 
        style(dirs_to_delete.len()).cyan()));
    
    Ok(dirs_to_delete)
}

fn delete_unwanted_folders(root_path: PathBuf, unwanted_folders: Arc<HashSet<String>>) -> io::Result<()> {
    let multi_progress = MultiProgress::new();
    
    // Сначала находим все директории для удаления (в одном потоке)
    let dirs_to_delete = collect_unwanted_directories(&root_path, &unwanted_folders)?;
    let total_dirs = dirs_to_delete.len();

    if total_dirs == 0 {
        println!("{}", style("Не найдено директорий для удаления.").yellow());
        return Ok(());
    }

    // Прогресс-бар для удаления
    let progress_bar = multi_progress.add(ProgressBar::new(total_dirs as u64));
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
            .unwrap()
            .progress_chars("=>-")
    );
    progress_bar.set_message("Удаление...");

    let deleted_count = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();

    // Лог операций
    let log_progress = multi_progress.add(ProgressBar::new_spinner());
    log_progress.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.yellow} {msg}")
            .unwrap()
    );
    
    let logger_handle = thread::spawn(move || {
        while let Ok(message) = rx.recv() {
            log_progress.set_message(message);
        }
        log_progress.finish_and_clear();
    });

    // Параллельное удаление найденных директорий
    dirs_to_delete.par_iter().for_each_with((tx.clone(), progress_bar.clone()), |(tx, pb), path| {
        match trash::delete(path) {
            Ok(_) => {
                deleted_count.fetch_add(1, Ordering::SeqCst);
                tx.send(format!("{} {}", 
                    style("✔").green(), 
                    style(format!("Удалена папка: {}", path.display())).dim()
                )).unwrap_or_default();
            },
            Err(e) => {
                tx.send(format!("{} Ошибка при удалении '{}': {}", 
                    style("✘").red(),
                    path.display(), 
                    style(e).red()
                )).unwrap_or_default();
                
                if let Err(e) = fs::remove_dir_all(path) {
                    tx.send(format!("{} Ошибка при полном удалении '{}': {}", 
                        style("✘").red(),
                        path.display(), 
                        style(e).red()
                    )).unwrap_or_default();
                } else {
                    deleted_count.fetch_add(1, Ordering::SeqCst);
                    tx.send(format!("{} {}", 
                        style("✔").green(), 
                        style(format!("Принудительно удалена папка: {}", path.display())).dim()
                    )).unwrap_or_default();
                }
            }
        }
        pb.inc(1);
    });

    progress_bar.finish_with_message(format!("Обработка завершена! Удалено папок: {}", 
        style(deleted_count.load(Ordering::SeqCst)).green()));

    drop(tx);
    logger_handle.join().unwrap();

    Ok(())
}

fn get_input(prompt: &str) -> String {
    print!("{}", prompt);
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    input.trim().to_string()
}

fn main() {
    // Устанавливаем количество потоков равное количеству ядер
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_cpus::get())
        .build_global()
        .unwrap();

    let args = Args::parse();

    // Получаем путь к конфигу
    let config_path = match args.config_path {
        Some(path) => path,
        None => get_input("Введите путь к файлу со списком нежелательных папок: "),
    };

    // Загружаем список нежелательных папок
    let unwanted_folders = match load_unwanted_folders(&config_path) {
        Ok(folders) => Arc::new(folders),
        Err(e) => {
            eprintln!("Ошибка при чтении файла с нежелательными папками: {}", e);
            return;
        }
    };

    // Получаем корневой путь
    let root_path = match args.root_path {
        Some(path) => path,
        None => get_input("Введите путь к корневой папке для очистки: "),
    };

    let path = Path::new(&root_path);
    if path.is_dir() {
        if let Err(e) = delete_unwanted_folders(path.to_path_buf(), unwanted_folders) {
            eprintln!("Ошибка при удалении папок: {}", e);
        } else {
            println!("Очистка завершена.");
        }
    } else {
        println!("Указанный путь не является директорией. Проверьте и попробуйте снова.");
    }
}
