use actix_cors::Cors;
use actix_multipart::Multipart;
use actix_web::{delete, get, http, post, web, App, HttpResponse, HttpServer, Responder};
use futures_util::StreamExt;
use futures_util::TryStreamExt;
use rusqlite::{params, Connection};
use sanitize_filename::sanitize;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{PathBuf, Path};
use std::sync::Mutex;
use std::env;

fn get_audio_upload_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));  // Ruta raíz del proyecto
    dir.push("uploads");
    dir
}

// Estructura para la respuesta del mensaje de éxito
#[derive(Serialize)]
struct UploadResponse {
    message: String,
    file_url: String,
}

// Estructura para recibir los metadatos
#[derive(Deserialize)]
struct SongMetadata {
    artist: String,
    title: String,
}

// Estructura para representar un track
#[derive(Serialize)]
struct Track {
    artist: String,
    title: String,
    file_url: String,
}

// Inicialización de la base de datos y creación de la tabla
fn init_db() -> Connection {
    let conn = Connection::open("tracks.db").expect("Failed to open database");
    conn.execute(
        "CREATE TABLE IF NOT EXISTS tracks (
            id INTEGER PRIMARY KEY,
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            file_path TEXT NOT NULL
        )",
        [],
    )
    .expect("Failed to create table");
    conn
}

// Handler para subir el archivo de audio y guardar en la base de datos
#[post("/upload")]
async fn upload(
    mut payload: Multipart,
    metadata: web::Query<SongMetadata>,
    db: web::Data<Mutex<Connection>>,
) -> impl Responder {
    // Obtener el directorio de subida de audios
    let audio_upload_dir = get_audio_upload_dir();

    // Crear el directorio si no existe
    fs::create_dir_all(&audio_upload_dir).unwrap();

    while let Ok(Some(mut field)) = payload.try_next().await {
        // Generar el nombre de archivo basado en {artista - título}
        let file_extension = "mp3"; // Asumiendo que todos los archivos son mp3
        let filename = format!(
            "{} - {}.{}",
            sanitize(&metadata.artist),
            sanitize(&metadata.title),
            file_extension
        );
        let filepath = audio_upload_dir.join(&filename);

        // Guardar el archivo subido
        let mut f = web::block(move || std::fs::File::create(filepath.clone()))
            .await
            .unwrap();

        while let Some(chunk) = field.next().await {
            let data = chunk.unwrap();
            f = web::block(move || {
                let mut file = f.unwrap(); // Desenrollamos el Result
                file.write_all(&data).map(|_| file)
            })
            .await
            .unwrap();
        }

        // Insertar los metadatos en la base de datos
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO tracks (artist, title, file_path) VALUES (?1, ?2, ?3)",
            params![&metadata.artist, &metadata.title, &filename],
        )
        .expect("Failed to insert track into database");

        // Devuelve la URL donde se podrá acceder al archivo
        let file_url = format!("/audio/{}", filename);

        let response = UploadResponse {
            message: String::from("File uploaded successfully"),
            file_url,
        };
        return HttpResponse::Ok().json(response);
    }

    HttpResponse::BadRequest().body("File upload failed")
}

// Handler para obtener los tracks
#[get("/tracks")]
async fn get_tracks(db: web::Data<Mutex<Connection>>) -> impl Responder {
    let conn = db.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT artist, title, file_path FROM tracks")
        .expect("Failed to prepare statement");

    let track_iter = stmt
        .query_map([], |row| {
            Ok(Track {
                artist: row.get(0)?,   // obtener el artista
                title: row.get(1)?,    // obtener el título
                file_url: format!("/audio/{}", row.get::<_, String>(2)?),  // obtener la URL del archivo
            })
        })
        .expect("Failed to map query");

    let mut tracks = Vec::new();
    for track in track_iter {
        tracks.push(track.unwrap());
    }

    HttpResponse::Ok().json(tracks)
}

// Handler para servir los archivos de audio
#[get("/audio/{filename}")]
async fn stream_audio(path: web::Path<String>) -> impl Responder {
    let filename = path.into_inner();
    let filepath: PathBuf = get_audio_upload_dir().join(&filename);
    if filepath.exists() {
        HttpResponse::Ok()
            .content_type("audio/mpeg")
            .insert_header(("Content-Disposition", "inline"))
            .body(fs::read(filepath).unwrap())
    } else {
        HttpResponse::NotFound().body("File not found")
    }
}

// Handler para eliminar un archivo y su registro en la base de datos
#[delete("/audio/{filename}")]
async fn delete_audio(path: web::Path<String>, db: web::Data<Mutex<Connection>>) -> impl Responder {
    let filename = path.into_inner();
    let filepath: PathBuf = get_audio_upload_dir().join(&filename);
    
    // Log para verificar la ruta
    println!("Attempting to delete file: {:?}", filepath);
    
    if filepath.exists() {
        // Eliminar el archivo del sistema
        fs::remove_file(filepath).unwrap();

        // Eliminar el registro de la base de datos
        let conn = db.lock().unwrap();
        conn.execute(
            "DELETE FROM tracks WHERE file_path = ?1",
            params![&filename],
        )
        .expect("Failed to delete track from database");

        HttpResponse::Ok().body("File and record deleted successfully")
    } else {
        HttpResponse::NotFound().body("File not found")
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let db = web::Data::new(Mutex::new(init_db())); // Conexión SQLite compartida

    HttpServer::new(move || {
        App::new()
            .app_data(db.clone())
            .wrap(
                Cors::default()
                    .allow_any_origin()
                    .allowed_methods(vec!["GET", "POST", "DELETE"])
                    .allowed_headers(vec![http::header::CONTENT_TYPE])
                    .max_age(3600),
            )
            .service(upload)
            .service(stream_audio)
            .service(delete_audio)
            .service(get_tracks) // Nuevo servicio para obtener las pistas
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
