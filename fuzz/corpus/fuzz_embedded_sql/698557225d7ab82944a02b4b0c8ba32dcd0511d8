 
BEGIN 
    INSERT INTO parent_procedure_templates (parent_id, child_id)
    VALUES (NEW.parent_id, NEW.predecessor_id) 
    ON CONFLICT (parent_id, child_id) DO NOTHING;

    INSERT INTO parent_procedure_templates (parent_id, child_id)
    VALUES (NEW.parent_id, NEW.successor_id) 
    ON CONFLICT (parent_id, child_id) DO NOTHING;
    
    RETURN NEW;
END;
