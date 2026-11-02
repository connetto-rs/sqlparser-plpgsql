
BEGIN
    WITH RECURSIVE parent_groups AS (
        SELECT parent_group_id AS id FROM groups WHERE id = NEW.group_id
        UNION ALL
        SELECT g.parent_group_id FROM groups g JOIN parent_groups pg ON g.id = pg.id
    )
    INSERT INTO group_memberships (group_id, user_id)
    SELECT id, NEW.user_id FROM parent_groups WHERE id IS NOT NULL
    ON CONFLICT (group_id, user_id) DO NOTHING;
    RETURN NEW;
END;
