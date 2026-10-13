-- PostgreSQL cannot remove a single enum value. Rollback requires an explicit
-- data decision for STALWART links and rebuilding both columns using this type.
DO $$ BEGIN
    RAISE EXCEPTION 'Cannot automatically remove STALWART from email_user_provider_enum';
END $$;
