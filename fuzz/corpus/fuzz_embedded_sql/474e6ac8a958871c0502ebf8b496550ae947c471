
BEGIN
    WITH RECURSIVE child_groups AS (
        SELECT id FROM groups WHERE parent_group_id = OLD.group_id
        UNION ALL
        SELECT g.id FROM groups g JOIN child_groups cg ON g.parent_group_id = cg.id
    )
    DELETE FROM group_memberships
    WHERE user_id = OLD.user_id
      AND group_id IN (SELECT id FROM child_groups);
    RETURN OLD;
END;
