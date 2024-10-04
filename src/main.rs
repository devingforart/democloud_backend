use actix_cors::Cors;
use actix_multipart::Multipart;
use actix_web::{delete, get, http, post, web, App, HttpResponse, HttpServer, Responder};
use futures_util::StreamExt;
use futures_util::TryStreamExt;
use rusqlite::{params, Connection};
use sanitize_filename::sanitize;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use uuid::Uuid;

fn get_audio_upload_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // Ruta raíz del proyecto
    dir.push("uploads");
    dir
}

#[derive(Serialize)]
struct UploadResponse {
    message: String,
    demo_id: String,  // Devolvemos el demo_id para generar la URL
    file_url: String, // También devolvemos el file_url para la previsualización
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
    demo_id: String,
}

// Inicialización de la base de datos y creación de la tabla
fn init_db() -> Connection {
    let conn = Connection::open("tracks.db").expect("Failed to open database");
    conn.execute(
        "CREATE TABLE IF NOT EXISTS tracks (
            id INTEGER PRIMARY KEY,
            artist TEXT NOT NULL,
            title TEXT NOT NULL,
            file_path TEXT NOT NULL,
            demo_id TEXT NOT NULL,
            user_id TEXT NOT NULL  -- Nuevo campo para almacenar el id de Auth0
        )",
        [],
    )
    .expect("Failed to create table");

    conn
}

#[post("/upload")]
async fn upload(
    mut payload: Multipart,
    metadata: web::Query<SongMetadata>,
    db: web::Data<Mutex<Connection>>,
    req: actix_web::HttpRequest, // Para obtener el user_id desde los headers
) -> impl Responder {
    let audio_upload_dir = get_audio_upload_dir();

    // Obtener el user_id del encabezado o del token JWT decodificado
    let user_id = match req.headers().get("user_id") {
        Some(value) => value.to_str().unwrap_or("").to_string(),
        None => return HttpResponse::BadRequest().body("Missing user_id in headers"),
    };
    // Intentamos crear el directorio de subida
    if let Err(e) = fs::create_dir_all(&audio_upload_dir) {
        eprintln!("Error creating upload directory: {:?}", e);
        return HttpResponse::InternalServerError()
            .body(format!("Failed to create upload directory: {:?}", e));
    }

    // Generar un UUID único para el demo
    let demo_id = Uuid::new_v4(); // Generamos un demo_id único

    while let Ok(Some(mut field)) = payload.try_next().await {
        let file_extension = "mp3";
        let filename = format!(
            "{}-{}.{}",
            sanitize(&metadata.title),
            demo_id,
            file_extension
        );
        let filepath = audio_upload_dir.join(&filename);

        // Manejar errores en la creación del archivo
        let mut f = match web::block(move || std::fs::File::create(filepath.clone())).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("Error creating file: {:?}", e);
                return HttpResponse::InternalServerError()
                    .body(format!("Failed to create file: {:?}", e));
            }
        };

        while let Some(chunk) = field.next().await {
            let data = match chunk {
                Ok(data) => data,
                Err(e) => {
                    eprintln!("Error reading chunk: {:?}", e);
                    return HttpResponse::InternalServerError()
                        .body(format!("Error reading file chunk: {:?}", e));
                }
            };

            // Manejar errores al escribir en el archivo
            f = match web::block(move || {
                let mut file = f.unwrap();
                file.write_all(&data).map(|_| file)
            })
            .await
            {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("Error writing file: {:?}", e);
                    return HttpResponse::InternalServerError()
                        .body(format!("Error writing file: {:?}", e));
                }
            };
        }

        // Insertar los datos en la base de datos con `user_id`
        let conn = match db.lock() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("Error locking database: {:?}", e);
                return HttpResponse::InternalServerError()
                    .body(format!("Failed to lock database: {:?}", e));
            }
        };

        if let Err(e) = conn.execute(
            "INSERT INTO tracks (artist, title, file_path, demo_id, user_id) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&metadata.artist, &metadata.title, &filename, demo_id.to_string(), &user_id],
        ) {
            eprintln!("Error inserting track into database: {:?}", e);
            return HttpResponse::InternalServerError()
                .body(format!("Failed to insert track into database: {:?}", e));
        }

        // Devolver la URL de la demo pública
        let response = UploadResponse {
            message: String::from("File uploaded successfully"),
            demo_id: demo_id.to_string(),
            file_url: format!("/audio/{}", filename),
        };
        return HttpResponse::Ok().json(response);
    }

    HttpResponse::BadRequest().body("File upload failed")
}

// Handler para obtener los tracks
#[get("/tracks")]
async fn get_tracks(
    db: web::Data<Mutex<Connection>>,
    req: actix_web::HttpRequest,
) -> impl Responder {
    let conn = match db.lock() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Error locking database: {:?}", e);
            return HttpResponse::InternalServerError().body("Failed to lock database");
        }
    };

    // Obtener el user_id del encabezado o del token JWT decodificado
    let user_id = match req.headers().get("user_id") {
        Some(value) => value.to_str().unwrap_or("").to_string(),
        None => return HttpResponse::BadRequest().body("Missing user_id in headers"),
    };

    let mut stmt = match conn
        .prepare("SELECT artist, title, file_path, demo_id FROM tracks WHERE user_id = ?1")
    {
        Ok(stmt) => stmt,
        Err(e) => {
            eprintln!("Error preparing SQL statement: {:?}", e);
            return HttpResponse::InternalServerError().body("Failed to prepare SQL statement");
        }
    };

    let track_iter = match stmt.query_map([&user_id], |row| {
        Ok(Track {
            artist: row.get(0)?,
            title: row.get(1)?,
            file_url: format!("/audio/{}", row.get::<_, String>(2)?),
            demo_id: row.get(3)?,
        })
    }) {
        Ok(track_iter) => track_iter,
        Err(e) => {
            eprintln!("Error mapping query: {:?}", e);
            return HttpResponse::InternalServerError().body("Failed to query tracks");
        }
    };

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

    if filepath.exists() {
        if let Err(e) = fs::remove_file(&filepath) {
            eprintln!("Error deleting file: {:?}", e);
            return HttpResponse::InternalServerError().body("Failed to delete file");
        }

        let conn = match db.lock() {
            Ok(conn) => conn,
            Err(e) => {
                eprintln!("Error locking database: {:?}", e);
                return HttpResponse::InternalServerError().body("Failed to lock database");
            }
        };

        if let Err(e) = conn.execute(
            "DELETE FROM tracks WHERE file_path = ?1",
            params![&filename],
        ) {
            eprintln!("Error deleting track from database: {:?}", e);
            return HttpResponse::InternalServerError()
                .body("Failed to delete track from database");
        }

        HttpResponse::Ok().body("File and record deleted successfully")
    } else {
        HttpResponse::NotFound().body("File not found")
    }
}

#[get("/demo/{demo_id}")]
async fn stream_demo(path: web::Path<String>, db: web::Data<Mutex<Connection>>) -> impl Responder {
    let demo_id = path.into_inner();
    let conn = match db.lock() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Error locking database: {:?}", e);
            return HttpResponse::InternalServerError().body("Failed to lock database");
        }
    };

    // Buscar el archivo basado en demo_id
    let mut stmt = match conn.prepare("SELECT file_path FROM tracks WHERE demo_id = ?1") {
        Ok(stmt) => stmt,
        Err(e) => {
            eprintln!("Error preparing SQL statement: {:?}", e);
            return HttpResponse::InternalServerError().body("Failed to prepare SQL statement");
        }
    };

    let result = stmt.query_row([&demo_id], |row| row.get::<_, String>(0)); // Especificamos que esperamos un String

    match result {
        Ok(file_path) => {
            let filepath = get_audio_upload_dir().join(file_path);
            if filepath.exists() {
                HttpResponse::Ok()
                    .content_type("audio/mpeg")
                    .insert_header(("Content-Disposition", "inline"))
                    .body(fs::read(filepath).unwrap())
            } else {
                HttpResponse::NotFound().body("File not found")
            }
        }
        Err(_) => HttpResponse::NotFound().body("Demo not found"),
    }
}

#[get("/demo_details/{demo_id}")]
async fn get_demo_details(
    path: web::Path<String>,
    db: web::Data<Mutex<Connection>>,
) -> impl Responder {
    let demo_id = path.into_inner();
    let conn = db.lock().unwrap();

    // Buscar el archivo basado en demo_id
    let mut stmt = conn
        .prepare("SELECT artist, title, file_path, demo_id FROM tracks WHERE demo_id = ?1")
        .expect("Failed to prepare statement");

    let result = stmt.query_row([&demo_id], |row| {
        Ok(Track {
            artist: row.get(0)?,
            title: row.get(1)?,
            file_url: format!("/audio/{}", row.get::<_, String>(2)?),
            demo_id: row.get(3)?, // Incluimos el demo_id aquí
        })
    });

    match result {
        Ok(track) => HttpResponse::Ok().json(track),
        Err(_) => HttpResponse::NotFound().body("Demo not found"),
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
                    .allowed_origin("https://test.devingfor.art") // Permite solicitudes desde localhost:5173
                    .allowed_methods(vec!["GET", "POST", "DELETE", "OPTIONS"]) // Permitir métodos específicos
                    .allowed_headers(vec![
                        http::header::CONTENT_TYPE,
                        http::header::AUTHORIZATION,
                        http::header::HeaderName::from_static("user_id"), // Convierte el nombre del encabezado
                    ])
                    .allow_any_header() // Permitir cualquier encabezado en las solicitudes
                    .supports_credentials() // Permitir el uso de cookies y credenciales en las solicitudes de CORS
                    .max_age(3600), // Cachea la respuesta preflight por 3600 segundos
            )
            .service(upload)
            .service(stream_audio)
            .service(delete_audio)
            .service(get_tracks)
            .service(stream_demo)
            .service(get_demo_details)
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}
