use std::{env, ffi::OsStr, path::Path, sync::Arc};

use apalis::{prelude::Storage, sqlite::SqliteStorage};
use async_graphql::{Context, Error, InputObject, Object, Result};
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QueryTrait,
};
use sea_query::{Condition, Expr, Func};
use serde::{Deserialize, Serialize};
use slug::slugify;

use crate::{
    background::UpdateExerciseJob,
    entities::{exercise, prelude::Exercise},
    file_storage::FileStorageService,
    models::{
        fitness::{Exercise as GithubExercise, ExerciseAttributes},
        SearchResults,
    },
    utils::{get_case_insensitive_like_query, PAGE_LIMIT},
};

#[derive(Debug, Serialize, Deserialize, InputObject, Clone)]
pub struct ExercisesListInput {
    pub page: i32,
    pub query: Option<String>,
}

#[derive(Default)]
pub struct ExerciseQuery;

#[Object]
impl ExerciseQuery {
    /// Get all the exercises in the database
    async fn exercises_list(
        &self,
        gql_ctx: &Context<'_>,
        input: ExercisesListInput,
    ) -> Result<SearchResults<exercise::Model>> {
        gql_ctx
            .data_unchecked::<Arc<ExerciseService>>()
            .exercises_list(input)
            .await
    }
}

#[derive(Default)]
pub struct ExerciseMutation;

#[Object]
impl ExerciseMutation {
    /// Deploy a job to download update the exercise library
    async fn deploy_update_exercise_library_job(&self, gql_ctx: &Context<'_>) -> Result<i32> {
        gql_ctx
            .data_unchecked::<Arc<ExerciseService>>()
            .deploy_update_exercise_library_job()
            .await
    }
}

#[derive(Debug)]
pub struct ExerciseService {
    db: DatabaseConnection,
    file_storage: Arc<FileStorageService>,
    json_url: String,
    image_prefix_url: String,
    update_exercise: SqliteStorage<UpdateExerciseJob>,
}

impl ExerciseService {
    pub fn new(
        db: &DatabaseConnection,
        file_storage: Arc<FileStorageService>,
        json_url: String,
        image_prefix_url: String,
        update_exercise: &SqliteStorage<UpdateExerciseJob>,
    ) -> Self {
        Self {
            db: db.clone(),
            file_storage,
            json_url,
            image_prefix_url,
            update_exercise: update_exercise.clone(),
        }
    }
}

impl ExerciseService {
    async fn get_all_exercises_from_dataset(&self) -> Result<Vec<GithubExercise>> {
        let data: Vec<GithubExercise> = surf::get(&self.json_url)
            .send()
            .await
            .unwrap()
            .body_json()
            .await
            .unwrap();
        Ok(data
            .into_iter()
            .map(|e| GithubExercise {
                attributes: ExerciseAttributes {
                    images: e
                        .attributes
                        .images
                        .into_iter()
                        .map(|i| format!("{}/{}", self.image_prefix_url, i))
                        .collect(),
                    ..e.attributes
                },
                ..e
            })
            .collect())
    }

    async fn exercises_list(
        &self,
        input: ExercisesListInput,
    ) -> Result<SearchResults<exercise::Model>> {
        let query = Exercise::find()
            .apply_if(input.query, |query, v| {
                query.filter(Condition::all().add(get_case_insensitive_like_query(
                    Func::lower(Expr::col(exercise::Column::Name)),
                    &v,
                )))
            })
            .order_by_asc(exercise::Column::Name);
        let total = query.clone().count(&self.db).await?;
        let total: i32 = total.try_into().unwrap();
        let data = query.paginate(&self.db, PAGE_LIMIT.try_into().unwrap());
        let mut resp = vec![];
        for ex in data
            .fetch_page((input.page - 1).try_into().unwrap())
            .await?
        {
            let mut ex_new = ex.clone();
            let mut images = vec![];
            for i in ex.attributes.images {
                let mut link = self.file_storage.get_presigned_url(i).await;
                // DEV: For the Expo app, since we are accessing the images on a
                // mobile device, we need to expose the minio instance and refer
                // to that in all images.
                if cfg!(feature = "development") {
                    let minio_url = env::var("S3_URL").unwrap();
                    let minio_public_url = env::var("S3_PUBLIC_URL").unwrap();
                    link = link.replace(&minio_url, &minio_public_url);
                    if let Some((m, _)) = link.split_once("?") {
                        link = m.to_owned();
                    }
                }
                images.push(link);
            }
            ex_new.attributes.images = images;
            resp.push(ex_new);
        }
        let next_page = if total - ((input.page) * PAGE_LIMIT) > 0 {
            Some(input.page + 1)
        } else {
            None
        };
        Ok(SearchResults {
            total,
            items: resp,
            next_page,
        })
    }

    async fn deploy_update_exercise_library_job(&self) -> Result<i32> {
        if !self.file_storage.is_enabled().await {
            return Err(Error::new(
                "File storage must be enabled for this feature.".to_owned(),
            ));
        }
        let mut storage = self.update_exercise.clone();
        let exercises = self.get_all_exercises_from_dataset().await?;
        let mut job_ids = vec![];
        for exercise in exercises {
            let job = storage.push(UpdateExerciseJob { exercise }).await?;
            job_ids.push(job.to_string());
        }
        Ok(job_ids.len().try_into().unwrap())
    }

    pub async fn update_exercise(&self, ex: GithubExercise) -> Result<()> {
        if Exercise::find()
            .filter(exercise::Column::Identifier.eq(&ex.identifier))
            .one(&self.db)
            .await?
            .is_none()
        {
            let mut images = vec![];
            let mut attributes = ex.attributes.clone();
            for (idx, image) in ex.attributes.images.into_iter().enumerate() {
                let ext = Path::new(&image)
                    .extension()
                    .and_then(OsStr::to_str)
                    .unwrap_or("png");
                let key = format!(
                    "fitness/exercises/{iden}/{idx}.{ext}",
                    iden = slugify(&ex.identifier)
                );
                let image_data = surf::get(image)
                    .send()
                    .await
                    .unwrap()
                    .body_bytes()
                    .await
                    .unwrap();
                images.push(key.clone());
                self.file_storage
                    .upload_file(&key, image_data.into())
                    .await?;
            }
            attributes.images = images;
            let db_exercise = exercise::ActiveModel {
                name: ActiveValue::Set(ex.name),
                identifier: ActiveValue::Set(ex.identifier),
                attributes: ActiveValue::Set(attributes),
                ..Default::default()
            };
            db_exercise.insert(&self.db).await?;
        }
        Ok(())
    }
}
