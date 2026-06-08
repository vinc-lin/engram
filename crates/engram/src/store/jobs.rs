use super::{enqueue_job_sql, now_secs, Store};
use crate::error::Result;
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone)]
pub struct Job {
    pub job_id: String,
    pub namespace: String,
    pub document_id: String,
    pub attempts: i64,
}

impl Store {
    pub fn enqueue_job(&self, namespace: &str, document_id: &str) -> Result<()> {
        let conn = self.write.lock().unwrap();
        enqueue_job_sql(&conn, namespace, document_id, now_secs())?;
        Ok(())
    }

    /// Atomically claim the oldest pending job → running (attempts incremented).
    pub fn claim_job(&self) -> Result<Option<Job>> {
        let mut conn = self.write.lock().unwrap();
        let tx = conn.transaction()?;
        let job = tx
            .query_row(
                "SELECT job_id, namespace, document_id, attempts FROM post_acquire_jobs
                 WHERE status='pending' ORDER BY created_at LIMIT 1",
                [],
                |r| {
                    Ok(Job {
                        job_id: r.get(0)?,
                        namespace: r.get(1)?,
                        document_id: r.get(2)?,
                        attempts: r.get(3)?,
                    })
                },
            )
            .optional()?;
        if let Some(ref j) = job {
            tx.execute(
                "UPDATE post_acquire_jobs SET status='running', attempts=attempts+1, updated_at=?2 WHERE job_id=?1",
                params![j.job_id, now_secs()],
            )?;
        }
        tx.commit()?;
        // reflect the increment we just committed
        Ok(job.map(|mut j| {
            j.attempts += 1;
            j
        }))
    }

    pub fn complete_job(&self, job_id: &str) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "UPDATE post_acquire_jobs SET status='done', last_error=NULL, updated_at=?2 WHERE job_id=?1",
            params![job_id, now_secs()],
        )?;
        Ok(())
    }

    /// Failed processing: back to `pending` for another try, or `failed` once attempts hit the cap.
    pub fn fail_or_retry_job(&self, job_id: &str, err: &str, max_attempts: i64) -> Result<()> {
        let conn = self.write.lock().unwrap();
        conn.execute(
            "UPDATE post_acquire_jobs
             SET status = CASE WHEN attempts >= ?3 THEN 'failed' ELSE 'pending' END,
                 last_error = ?2, updated_at = ?4
             WHERE job_id = ?1",
            params![job_id, err, max_attempts, now_secs()],
        )?;
        Ok(())
    }

    /// Crash recovery: any job left `running` returns to `pending`.
    pub fn requeue_running(&self) -> Result<usize> {
        let conn = self.write.lock().unwrap();
        let n = conn.execute(
            "UPDATE post_acquire_jobs SET status='pending' WHERE status='running'",
            [],
        )?;
        Ok(n)
    }

    pub fn job(&self, job_id: &str) -> Result<Option<(String, i64)>> {
        let conn = self.read.get()?;
        Ok(conn
            .query_row(
                "SELECT status, attempts FROM post_acquire_jobs WHERE job_id=?1",
                params![job_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn pending_jobs(&self) -> Result<i64> {
        let conn = self.read.get()?;
        Ok(conn.query_row(
            "SELECT count(*) FROM post_acquire_jobs WHERE status='pending'",
            [],
            |r| r.get(0),
        )?)
    }
}
