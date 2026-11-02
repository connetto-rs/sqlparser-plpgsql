 
BEGIN
    INSERT INTO procedure_template_asset_models (
            name,
            procedure_template_id,
            based_on_id,
            asset_model_id
        )
    SELECT 
        pam.name,
        NEW.parent_id,
        pam.id,
        pam.asset_model_id
    FROM procedure_template_asset_models pam
    WHERE pam.procedure_template_id = NEW.child_id;
    
    RETURN NEW;
END;
