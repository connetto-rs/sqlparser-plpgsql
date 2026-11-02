
BEGIN
    WITH RECURSIVE chain AS (
        SELECT id, parent_id FROM tree_nodes WHERE id = NEW.id
        UNION ALL
        SELECT c.id, t.parent_id FROM chain c JOIN tree_nodes t ON c.parent_id = t.id WHERE t.parent_id IS NOT NULL
    )
    INSERT INTO node_ancestors (node_id, ancestor_id)
    SELECT NEW.id, parent_id FROM chain WHERE parent_id IS NOT NULL;

    RETURN NEW;
END;
